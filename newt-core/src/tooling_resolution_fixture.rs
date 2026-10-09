//! Scoped, thread-local resolver injection for deterministic dispatch races.
use std::cell::RefCell;

type Resolver = Box<dyn FnMut() -> Vec<String>>;
thread_local! {
    static RESOLVER: RefCell<Option<Resolver>> = const { RefCell::new(None) };
}

pub(crate) struct Override;
impl Override {
    pub(crate) fn install(resolver: impl FnMut() -> Vec<String> + 'static) -> Self {
        RESOLVER.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(resolver));
        });
        Self
    }
}
impl Drop for Override {
    fn drop(&mut self) {
        RESOLVER.with(|slot| *slot.borrow_mut() = None);
    }
}
pub(super) fn resolve() -> Option<Vec<String>> {
    RESOLVER.with(|slot| slot.borrow_mut().as_mut().map(|resolve| resolve()))
}
