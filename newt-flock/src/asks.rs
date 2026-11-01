//! The verified open asks, as the operator's screen shows them.

use agent_mesh_flock::text::inert;
use agent_mesh_flock::{Board, Body, Kind};
use content_addressable::ContentId;
use std::collections::BTreeMap;

/// An ask in the verified log that nothing has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAsk {
    /// The ask's content id.
    pub id: ContentId,
    /// The member that asked.
    pub from: String,
    /// The lane it concerns, if any.
    pub lane: Option<String>,
    /// The question, exactly as logged. Render it with [`Self::line`].
    pub text: String,
}

impl OpenAsk {
    /// One display line with member text escaped, so a hostile ask cannot
    /// forge terminal output.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{} {} [{}] {}",
            self.id,
            inert(&self.from),
            inert(self.lane.as_deref().unwrap_or("-")),
            inert(&self.text)
        )
    }
}

/// What the verified board says about asks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskView {
    /// Asks nothing has closed, in admission order.
    pub open: Vec<OpenAsk>,
    /// Asks that a closer answered: ask id to the closing event's id.
    pub answered: BTreeMap<ContentId, ContentId>,
}

/// The ask view of a verified board.
#[must_use]
pub fn view(board: &Board) -> AskView {
    fold(
        board
            .entries()
            .filter(|e| e.event.kind == Kind::Ask)
            .filter_map(|e| {
                let Body::Ask { lane, text } = &e.event.body else {
                    return None;
                };
                let ask = OpenAsk {
                    id: e.id,
                    from: e.member.clone().unwrap_or_default(),
                    lane: lane.clone(),
                    text: text.clone(),
                };
                Some((ask, e.conflict.is_some(), board.closer_of(&e.id)))
            }),
    )
}

/// The fold over `(ask, conflicted, closer)` in admission order: a conflicted
/// ask is not shown, a closed one is answered, the rest are open.
fn fold(asks: impl IntoIterator<Item = (OpenAsk, bool, Option<ContentId>)>) -> AskView {
    let mut v = AskView::default();
    for (ask, conflicted, closer) in asks {
        if conflicted {
            continue;
        }
        match closer {
            Some(by) => {
                v.answered.insert(ask.id, by);
            }
            None => v.open.push(ask),
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ContentId {
        ContentId::from_dag_cbor_digest([n; 32])
    }

    fn ask(n: u8) -> OpenAsk {
        OpenAsk {
            id: id(n),
            from: "h1".into(),
            lane: None,
            text: "q".into(),
        }
    }

    #[test]
    fn a_conflicted_ask_is_not_shown_and_a_closed_one_is_answered_not_open() {
        let v = fold([
            (ask(1), false, None),
            (ask(2), true, None),
            (ask(3), false, Some(id(9))),
            (ask(4), true, Some(id(8))),
            (ask(5), false, None),
        ]);
        let open: Vec<_> = v.open.iter().map(|a| a.id).collect();
        assert_eq!(
            open,
            [id(1), id(5)],
            "admission order, no conflict, no closed"
        );
        assert_eq!(v.answered.len(), 1);
        assert_eq!(v.answered.get(&id(3)), Some(&id(9)));
    }
}
