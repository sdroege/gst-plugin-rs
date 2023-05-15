use rayon::{ThreadPool, ThreadPoolBuilder};

use std::sync::{Arc, LazyLock, Mutex, Weak};

static THREAD_POOL: LazyLock<Mutex<Weak<ThreadPool>>> = LazyLock::new(|| Mutex::new(Weak::new()));

pub fn thread_pool() -> Result<Arc<ThreadPool>, gst::ErrorMessage> {
    let mut thread_pool = THREAD_POOL.lock().unwrap();

    if let Some(tp) = thread_pool.upgrade() {
        Ok(tp)
    } else {
        let tp = Arc::new(ThreadPoolBuilder::new().build().map_err(|_| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Could not create rayon thread pool"]
            )
        })?);

        *thread_pool = Arc::downgrade(&tp);
        Ok(tp)
    }
}
