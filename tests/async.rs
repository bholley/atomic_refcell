use std::future::Future;

use atomic_refcell::AtomicRefCell;

fn spawn<Fut>(_future: Fut)
where
    Fut: Future<Output = ()> + Send + Sync,
{
}

async fn something_async() {}

// see https://github.com/bholley/atomic_refcell/issues/24
#[test]
fn test_atomic_ref_in_spawn() {
    let arc: Box<dyn Fn() -> usize + Send + Sync> = Box::new(|| 42);
    let a = AtomicRefCell::new(arc);
    spawn(async move {
        let x = a.borrow();
        something_async().await;
        assert_eq!(x(), 42);
    });
}

#[test]
fn test_atomic_ref_mut_in_spawn() {
    let arc: Box<dyn Fn() -> usize + Send + Sync> = Box::new(|| 42);
    let a = AtomicRefCell::new(arc);
    spawn(async move {
        let x = a.borrow_mut();
        something_async().await;
        assert_eq!(x(), 42);
    });
}
