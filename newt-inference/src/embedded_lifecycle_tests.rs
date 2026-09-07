use super::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

const WATCHDOG: Duration = Duration::from_secs(5);

#[tokio::test]
async fn expired_generation_does_not_start_the_loader() {
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let result = run_generation(
        Arc::new(tokio::sync::Semaphore::new(1)),
        Some(Instant::now()),
        move |_checkpoint| {
            worker_started.store(true, Ordering::SeqCst);
            Ok("unexpected generation".into())
        },
    )
    .await;
    assert!(result.is_err(), "an expired deadline must fail: {result:?}");
    assert!(
        !started.load(Ordering::SeqCst),
        "expired work loaded a model"
    );
}

/// A real blocking worker grounds the mocked generation checkpoints: dropping
/// the async caller must stop the next step, without freeing its slot early.
#[tokio::test]
async fn dropped_caller_keeps_admission_until_the_worker_exits() {
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (exited_tx, exited_rx) = tokio::sync::oneshot::channel();
    let continued = Arc::new(AtomicBool::new(false));
    let worker_continued = continued.clone();
    let first = tokio::spawn(run_generation(admission.clone(), None, move |checkpoint| {
        let _ = started_tx.send(());
        let result = (|| {
            release_rx.recv_timeout(WATCHDOG)?;
            checkpoint()?;
            worker_continued.store(true, Ordering::SeqCst);
            Ok("first".into())
        })();
        let _ = exited_tx.send(());
        result
    }));
    tokio::time::timeout(WATCHDOG, started_rx)
        .await
        .unwrap()
        .unwrap();
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());

    let second_started = Arc::new(AtomicBool::new(false));
    let worker_second = second_started.clone();
    let second = tokio::time::timeout(
        WATCHDOG,
        run_generation(admission.clone(), None, move |_| {
            worker_second.store(true, Ordering::SeqCst);
            Ok("second".into())
        }),
    )
    .await
    .unwrap();
    // Release the fixture before assertions, including on the original broken
    // implementation. Dropping release_tx on panic also unblocks the worker.
    let _ = release_tx.send(());
    tokio::time::timeout(WATCHDOG, exited_rx)
        .await
        .unwrap()
        .unwrap();
    let permit = tokio::time::timeout(WATCHDOG, admission.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
    assert!(
        second.is_err(),
        "a cancelled-but-running worker admitted a successor: {second:?}"
    );
    assert!(!second_started.load(Ordering::SeqCst));
    assert!(
        !continued.load(Ordering::SeqCst),
        "work continued past a cancellation checkpoint"
    );
    assert_eq!(
        run_generation(admission, None, |_| Ok("after exit".into()))
            .await
            .unwrap(),
        "after exit"
    );
}

#[tokio::test]
async fn deadline_returns_while_the_current_step_still_owns_admission() {
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let continued = Arc::new(AtomicBool::new(false));
    let worker_continued = continued.clone();
    let first = tokio::spawn(run_generation(
        admission.clone(),
        Some(Instant::now() + Duration::from_secs(60)),
        move |checkpoint| {
            let _ = started_tx.send(());
            release_rx.recv_timeout(WATCHDOG)?;
            checkpoint()?;
            worker_continued.store(true, Ordering::SeqCst);
            Ok("unexpected generation".into())
        },
    ));
    tokio::time::timeout(WATCHDOG, started_rx)
        .await
        .unwrap()
        .unwrap();
    // Advance the async clock only after real worker readiness; its startup is
    // not required to beat a short timer on a contended test machine.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    let result = tokio::time::timeout(Duration::from_secs(1), first).await;
    let held = admission.available_permits() == 0;
    let _ = release_tx.send(());
    let permit = tokio::time::timeout(WATCHDOG, admission.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(permit);
    assert!(
        result.is_ok(),
        "deadline did not return while a synchronous step was held"
    );
    assert!(result.unwrap().unwrap().is_err());
    assert!(held, "timeout released a still-running worker's permit");
    assert!(!continued.load(Ordering::SeqCst));
}

#[tokio::test]
async fn error_and_panic_release_generation_admission() {
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    assert!(
        run_generation(admission.clone(), None, |_| anyhow::bail!("fixture error"))
            .await
            .is_err()
    );
    assert!(
        run_generation(admission.clone(), None, |_| panic!("fixture panic"))
            .await
            .is_err()
    );
    assert_eq!(
        run_generation(admission, None, |_| Ok("recovered".into()))
            .await
            .unwrap(),
        "recovered"
    );
}

#[test]
fn prefill_visits_each_token_without_sampling_or_treating_prompt_eos_as_output() {
    use std::cell::RefCell;
    let events = RefCell::new(Vec::new());
    let mut samples = [17, 99].into_iter();
    let tokens = engine::generate_tokens(
        vec![3, 99, 5],
        99,
        4,
        &|| Ok(()),
        |tokens, pos| {
            events
                .borrow_mut()
                .push(format!("forward {tokens:?} at {pos}"));
            Ok(candle_core::Tensor::new(
                &[1f32],
                &candle_core::Device::Cpu,
            )?)
        },
        |_| {
            events.borrow_mut().push("sample".into());
            Ok(samples.next().unwrap())
        },
    )
    .unwrap();
    assert_eq!(tokens, [17]);
    assert_eq!(
        *events.borrow(),
        [
            "forward [3] at 0",
            "forward [99] at 1",
            "forward [5] at 2",
            "sample",
            "forward [17] at 3",
            "sample",
        ]
    );
}

#[test]
fn prefill_stops_before_the_next_forward_after_cancellation() {
    use std::cell::Cell;
    let forwards = Cell::new(0);
    let result = engine::generate_tokens(
        vec![3, 4, 5],
        99,
        4,
        &|| {
            anyhow::ensure!(forwards.get() < 1, "cancelled");
            Ok(())
        },
        |_, _| {
            forwards.set(forwards.get() + 1);
            Ok(candle_core::Tensor::new(
                &[1f32],
                &candle_core::Device::Cpu,
            )?)
        },
        |_| panic!("cancelled prefill must never sample"),
    );
    assert!(result.is_err());
    assert_eq!(forwards.get(), 1);
}

#[test]
fn zero_output_budget_does_no_forward_or_sampling() {
    let result = engine::generate_tokens(
        vec![3],
        99,
        0,
        &|| Ok(()),
        |_, _| panic!("zero-budget forward"),
        |_| panic!("zero-budget sample"),
    )
    .unwrap();
    assert!(result.is_empty());
}

/// Real Candle attention/KV-cache arithmetic grounds the mocked token schedule.
/// This tiny, nontrivial one-block GGUF is synthesized locally: no downloaded
/// weights, network access, or claims about language-model quality are needed.
#[test]
fn token_schedule_preserves_real_qwen_prefill_and_continuation_logits() {
    use candle_core::{Device, Tensor};
    let mut batched = tiny_qwen();
    let mut scheduled = tiny_qwen();
    let prompt = [3, 4, 5];
    let mut expected = Vec::new();
    for (tokens, pos) in [(prompt.as_slice(), 0), (&[7][..], 3), (&[11][..], 4)] {
        let input = Tensor::new(tokens, &Device::Cpu)
            .unwrap()
            .unsqueeze(0)
            .unwrap();
        expected.push(
            batched
                .forward(&input, pos)
                .unwrap()
                .squeeze(0)
                .unwrap()
                .to_vec1::<f32>()
                .unwrap(),
        );
    }
    let mut actual = Vec::new();
    let mut samples = [7, 11, 31].into_iter();
    engine::generate_tokens(
        prompt.to_vec(),
        31,
        3,
        &|| Ok(()),
        |tokens, pos| {
            let input = Tensor::new(tokens, &Device::Cpu)?.unsqueeze(0)?;
            let logits = scheduled.forward(&input, pos)?.squeeze(0)?;
            if pos + tokens.len() >= prompt.len() {
                actual.push(logits.to_vec1::<f32>()?);
            }
            Ok(logits)
        },
        |_| Ok(samples.next().unwrap()),
    )
    .unwrap();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(&expected) {
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected).abs() < 0.0002,
                "prefill/continuation logits diverged: {actual} vs {expected}"
            );
        }
    }
}

fn tiny_qwen() -> candle_transformers::models::quantized_qwen2::ModelWeights {
    use candle_core::quantized::{gguf_file, GgmlDType, QTensor};
    use candle_core::{Device, Tensor};
    let metadata = [
        ("qwen2.attention.head_count", gguf_file::Value::U32(1)),
        ("qwen2.attention.head_count_kv", gguf_file::Value::U32(1)),
        ("qwen2.embedding_length", gguf_file::Value::U32(32)),
        ("qwen2.context_length", gguf_file::Value::U32(32)),
        ("qwen2.block_count", gguf_file::Value::U32(1)),
        (
            "qwen2.attention.layer_norm_rms_epsilon",
            gguf_file::Value::F32(0.00001),
        ),
    ];
    let mut tensors = Vec::new();
    for (seed, name) in [
        "token_embd.weight",
        "output.weight",
        "blk.0.attn_q.weight",
        "blk.0.attn_k.weight",
        "blk.0.attn_v.weight",
        "blk.0.attn_output.weight",
        "blk.0.ffn_gate.weight",
        "blk.0.ffn_down.weight",
        "blk.0.ffn_up.weight",
    ]
    .into_iter()
    .enumerate()
    {
        let values: Vec<f32> = (0..1024)
            .map(|i| (((i * 37 + seed * 17) % 101) as f32 - 50.) * 0.002)
            .collect();
        let tensor = Tensor::from_vec(values, (32, 32), &Device::Cpu).unwrap();
        tensors.push((name, QTensor::quantize(&tensor, GgmlDType::Q4_0).unwrap()));
    }
    for name in [
        "output_norm.weight",
        "blk.0.attn_norm.weight",
        "blk.0.ffn_norm.weight",
    ] {
        let tensor = Tensor::new(&[1f32; 32], &Device::Cpu).unwrap();
        tensors.push((name, QTensor::quantize(&tensor, GgmlDType::F32).unwrap()));
    }
    for name in [
        "blk.0.attn_q.bias",
        "blk.0.attn_k.bias",
        "blk.0.attn_v.bias",
    ] {
        let tensor = Tensor::new(&[0.01f32; 32], &Device::Cpu).unwrap();
        tensors.push((name, QTensor::quantize(&tensor, GgmlDType::F32).unwrap()));
    }
    let metadata: Vec<_> = metadata
        .iter()
        .map(|(name, value)| (*name, value))
        .collect();
    let tensors: Vec<_> = tensors
        .iter()
        .map(|(name, tensor)| (*name, tensor))
        .collect();
    let mut file = std::io::Cursor::new(Vec::new());
    gguf_file::write(&mut file, &metadata, &tensors).unwrap();
    file.set_position(0);
    let content = gguf_file::Content::read(&mut file).unwrap();
    candle_transformers::models::quantized_qwen2::ModelWeights::from_gguf(
        content,
        &mut file,
        &Device::Cpu,
    )
    .unwrap()
}
