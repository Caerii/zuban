use std::{cell::Cell, panic::PanicHookInfo, sync::Once};

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + 'static>;

thread_local! {
    static HOOK: Cell<Option<PanicHook>> = const { Cell::new(None) };
}

pub struct RestorePanicHook(Option<PanicHook>);

impl Drop for RestorePanicHook {
    fn drop(&mut self) {
        HOOK.with(|cell| {
            cell.take();
            cell.set(self.0.take());
        });
    }
}

pub fn enter(new_hook: PanicHook) -> RestorePanicHook {
    // Server restarts should not wrap the process-global hook repeatedly.
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            HOOK.with(|cell| match cell.take() {
                Some(new_hook) => {
                    new_hook(panic_info);
                    cell.set(Some(new_hook));
                }
                None => default_hook(panic_info),
            })
        }));
    });
    RestorePanicHook(HOOK.with(|cell| cell.replace(Some(new_hook))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn nested_handlers_restore_the_outer_handler() {
        static OUTER: AtomicUsize = AtomicUsize::new(0);
        static INNER: AtomicUsize = AtomicUsize::new(0);

        let outer = enter(Box::new(|_| {
            OUTER.fetch_add(1, Ordering::Relaxed);
        }));
        {
            let _inner = enter(Box::new(|_| {
                INNER.fetch_add(1, Ordering::Relaxed);
            }));
            let _ = std::panic::catch_unwind(|| panic!("inner"));
            let _ = std::panic::catch_unwind(|| panic!("inner again"));
        }
        let _ = std::panic::catch_unwind(|| panic!("outer"));
        drop(outer);

        assert_eq!(INNER.load(Ordering::Relaxed), 2);
        assert_eq!(OUTER.load(Ordering::Relaxed), 1);
        assert!(HOOK.with(|cell| cell.take().is_none()));
    }
}
