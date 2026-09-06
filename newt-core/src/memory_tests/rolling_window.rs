use super::*;

#[tokio::test]
async fn rolling_window_empty_produces_two_messages() {
    let rw = RollingWindow::new(5);
    let msgs = rw.build_messages("sys", "hello");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(msgs[1].content, "hello");
}
#[tokio::test]
async fn rolling_window_includes_history() {
    let mut rw = RollingWindow::new(5);
    rw.sync_turn("q1", "a1", &dummy_metrics()).await;
    rw.sync_turn("q2", "a2", &dummy_metrics()).await;
    let msgs = rw.build_messages("sys", "q3");
    // system + (q1,a1) + (q2,a2) + q3 = 6
    assert_eq!(msgs.len(), 6);
    assert_eq!(msgs[1].content, "q1");
    assert_eq!(msgs[2].content, "a1");
    assert_eq!(msgs[5].content, "q3");
}
#[tokio::test]
async fn rolling_window_caps_at_max_turns() {
    let mut rw = RollingWindow::new(2);
    for i in 0..5u32 {
        rw.sync_turn(&format!("q{i}"), &format!("a{i}"), &dummy_metrics())
            .await;
    }
    let msgs = rw.build_messages("sys", "q5");
    // system + 2 turns * 2 messages + current = 6
    assert_eq!(msgs.len(), 6);
    // The last 2 turns should be q3/a3 and q4/a4
    assert_eq!(msgs[1].content, "q3");
    assert_eq!(msgs[3].content, "q4");
    assert_eq!(msgs[5].content, "q5");
}
#[tokio::test]
async fn rolling_window_usage_reports_correctly() {
    let mut rw = RollingWindow::new(10);
    rw.sync_turn("q", "a", &dummy_metrics()).await;
    rw.sync_turn("q", "a", &dummy_metrics()).await;
    let (label, cur, max) = rw.usage().unwrap();
    assert_eq!(label, "turns");
    assert_eq!(cur, 2);
    assert_eq!(max, 10);
}
#[tokio::test]
async fn rolling_window_on_session_end_noop() {
    let mut rw = RollingWindow::new(5);
    rw.on_session_end(&[]).await; // must not panic
}
