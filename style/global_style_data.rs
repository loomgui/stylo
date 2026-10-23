/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Global style data

use crate::context::StyleSystemOptions;
#[cfg(feature = "gecko")]
use crate::gecko_bindings::bindings;
use crate::parallel::STYLE_THREAD_STACK_SIZE_KB;
use crate::shared_lock::SharedRwLock;
use crate::thread_state;
use parking_lot::{Mutex, RwLock, RwLockReadGuard};
#[cfg(all(unix, not(target_arch = "wasm32")))]
use std::os::unix::thread::{JoinHandleExt, RawPthread};
#[cfg(windows)]
use std::os::windows::{io::AsRawHandle, prelude::RawHandle};
use std::{io, sync::LazyLock, thread};
use thin_vec::ThinVec;

/// Platform-specific handle to a thread.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub type PlatformThreadHandle = RawPthread;
/// Platform-specific handle to a thread.
#[cfg(windows)]
pub type PlatformThreadHandle = RawHandle;

/// A noop thread join handle for wasm
/// The usize field is a dummy field to make this type non-zero sized so as not to confuse FFI
#[cfg(all(target_arch = "wasm32", not(feature = "gecko")))]
pub struct DummyThreadHandle;
#[cfg(all(target_arch = "wasm32", not(feature = "gecko")))]
impl DummyThreadHandle {
    /// A noop thread join method for wasm
    pub fn join(&self) {
        // Do nothing
    }
}
#[cfg(all(target_arch = "wasm32", not(feature = "gecko")))]
/// Platform-specific handle to a thread.
pub type PlatformThreadHandle = DummyThreadHandle;

/// Global style data
pub struct GlobalStyleData {
    /// Shared RWLock for CSSOM objects
    pub shared_lock: SharedRwLock,

    /// Global style system options determined by env vars.
    pub options: StyleSystemOptions,
}

/// Global thread pool.
pub struct StyleThreadPool {
    /// How many threads parallel styling can use. If not using a thread pool, this is set to `None`.
    pub num_threads: Option<usize>,

    /// The parallel styling thread pool.
    ///
    /// For leak-checking purposes, we want to terminate the thread-pool, which
    /// waits for all the async jobs to complete. Thus the RwLock.
    style_thread_pool: RwLock<Option<rayon::ThreadPool>>,
}

fn thread_name(index: usize) -> String {
    format!("StyleThread#{}", index)
}

/// JoinHandles for spawned style threads. These will be joined during
/// StyleThreadPool::shutdown() after exiting the thread pool.
///
/// This would be quite inefficient if rayon destroyed and re-created
/// threads regularly during threadpool operation in response to demand,
/// however rayon actually never destroys its threads until the entire
/// thread pool is shut-down, so the size of this list is bounded.
static STYLE_THREAD_JOIN_HANDLES: Mutex<Vec<thread::JoinHandle<()>>> = Mutex::new(Vec::new());

fn spawn_style_thread(options: rayon::ThreadBuilder) -> io::Result<thread::JoinHandle<()>> {
    let mut b = thread::Builder::new();
    if let Some(name) = options.name() {
        b = b.name(name.to_owned());
    }
    if let Some(stack_size) = options.stack_size() {
        b = b.stack_size(stack_size);
    }
    b.spawn(|| options.run())
}

fn thread_spawn(options: rayon::ThreadBuilder) -> io::Result<()> {
    let join_handle = spawn_style_thread(options)?;
    STYLE_THREAD_JOIN_HANDLES.lock().push(join_handle);
    Ok(())
}

fn thread_startup(_index: usize) {
    thread_state::initialize_layout_worker_thread();
    #[cfg(feature = "gecko")]
    unsafe {
        bindings::Gecko_SetJemallocThreadLocalArena(true);
        let name = thread_name(_index);
        gecko_profiler::register_thread(&name);
    }
}

fn thread_shutdown(_: usize) {
    #[cfg(feature = "gecko")]
    unsafe {
        gecko_profiler::unregister_thread();
        bindings::Gecko_SetJemallocThreadLocalArena(false);
    }
}

impl StyleThreadPool {
    /// Shuts down the thread pool, waiting for all work to complete.
    pub fn shutdown() {
        if STYLE_THREAD_JOIN_HANDLES.lock().is_empty() {
            return;
        }
        {
            // Drop the pool.
            let _ = STYLE_THREAD_POOL.style_thread_pool.write().take();
        }

        // Join spawned threads until all of the threads have been joined. This
        // will usually be pretty fast, as on shutdown there should be basically
        // no threads left running.
        while let Some(join_handle) = STYLE_THREAD_JOIN_HANDLES.lock().pop() {
            let _ = join_handle.join();
        }
    }

    /// Returns a reference to the thread pool.
    ///
    /// We only really want to give read-only access to the pool, except
    /// for shutdown().
    pub fn pool(&self) -> RwLockReadGuard<'_, Option<rayon::ThreadPool>> {
        self.style_thread_pool.read()
    }

    /// Returns a list of the pool's platform-specific thread handles.
    pub fn get_thread_handles(handles: &mut ThinVec<PlatformThreadHandle>) {
        // Force the lazy initialization of STYLE_THREAD_POOL so that the threads get spawned and
        // their join handles are added to STYLE_THREAD_JOIN_HANDLES.
        LazyLock::force(&STYLE_THREAD_POOL);

        for join_handle in STYLE_THREAD_JOIN_HANDLES.lock().iter() {
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            let handle = join_handle.as_pthread_t();
            #[cfg(windows)]
            let handle = join_handle.as_raw_handle();
            #[cfg(all(target_arch = "wasm32", not(feature = "gecko")))]
            let handle = {
                let _ = join_handle;
                DummyThreadHandle
            };

            handles.push(handle);
        }
    }
}

#[cfg(feature = "servo")]
fn stylo_threads_pref() -> i32 {
    crate::pref!("layout.threads")
}

#[cfg(feature = "gecko")]
fn stylo_threads_pref() -> i32 {
    crate::pref!("layout.css.stylo-threads")
}

/// The performance benefit of additional threads seems to level off at around six, so we cap it
/// there on many-core machines (see bug 1431285 comment 14).
pub(crate) const STYLO_MAX_THREADS: usize = 6;

fn build_style_thread_pool<F>(
    num_threads: usize,
    spawn_handler: F,
) -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError>
where
    F: FnMut(rayon::ThreadBuilder) -> io::Result<()>,
{
    let builder = rayon::ThreadPoolBuilder::new()
        .spawn_handler(spawn_handler)
        .num_threads(num_threads)
        .thread_name(thread_name)
        .start_handler(thread_startup)
        .exit_handler(thread_shutdown)
        .stack_size(STYLE_THREAD_STACK_SIZE_KB * 1024);

    // Gecko enters traversal on the main thread that owns pool index zero.
    #[cfg(feature = "gecko")]
    let builder = builder.use_current_thread();

    // Servo can create this process-global pool while loading an earlier document.
    // Every configured worker must remain available after that script thread exits.
    builder.build()
}

/// Global thread pool
pub static STYLE_THREAD_POOL: LazyLock<StyleThreadPool> = LazyLock::new(|| {
    use std::cmp;
    // We always set this pref on startup, before layout or script have had a chance of
    // accessing (and thus creating) the thread-pool.
    let threads_pref: i32 = stylo_threads_pref();
    let num_threads = if threads_pref >= 0 {
        threads_pref as usize
    } else {
        // Gecko may wish to override the default number of threads, for example on
        // systems with heterogeneous CPUs.
        #[cfg(feature = "gecko")]
        let num_threads = unsafe { bindings::Gecko_GetNumStyleThreads() };
        #[cfg(not(feature = "gecko"))]
        let num_threads = -1;

        if num_threads >= 0 {
            num_threads as usize
        } else {
            // The default heuristic is num_virtual_cores * .75. This gives us three threads on a
            // hyper-threaded dual core, and six threads on a hyper-threaded quad core.
            cmp::max(num_cpus::get() * 3 / 4, 1)
        }
    };

    let num_threads = cmp::min(num_threads, STYLO_MAX_THREADS);
    // Retain the existing serial mode when parallel styling is disabled or only
    // one thread is requested.
    let (pool, num_threads) = if num_threads <= 1 {
        (None, None)
    } else {
        let workers = build_style_thread_pool(num_threads, thread_spawn);
        (workers.ok(), Some(num_threads))
    };

    StyleThreadPool {
        num_threads,
        style_thread_pool: RwLock::new(pool),
    }
});

/// Global style data
pub static GLOBAL_STYLE_DATA: LazyLock<GlobalStyleData> = LazyLock::new(|| GlobalStyleData {
    shared_lock: SharedRwLock::new_leaked(),
    options: StyleSystemOptions::default(),
});

#[cfg(all(test, feature = "servo", not(feature = "gecko")))]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    fn join_bounded<T>(handle: thread::JoinHandle<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            handle.is_finished(),
            "style-pool test thread must terminate"
        );
        handle.join().expect("style-pool test thread panicked")
    }

    #[test]
    fn all_configured_style_workers_survive_the_pool_creator() {
        // Exercise the same builder, spawn implementation and lifecycle hooks as
        // the global pool, with private join handles to isolate other unit tests.
        let handles = Arc::new(Mutex::new(Vec::new()));
        let creator_handles = handles.clone();
        let (created_tx, created_rx) = mpsc::channel();
        let creator = thread::spawn(move || {
            let creator_id = thread::current().id();
            let pool = build_style_thread_pool(3, move |options| {
                creator_handles.lock().push(spawn_style_thread(options)?);
                Ok(())
            })
            .expect("create the production style pool");
            created_tx.send((creator_id, pool)).unwrap();
        });
        let (creator_id, pool) = created_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the production style pool must finish construction");
        join_bounded(creator);
        assert_eq!(pool.current_num_threads(), 3);

        // Unlike broadcast(), this cannot wait forever for a reserved slot whose
        // creator has exited. The old use_current_thread path reports only 1/2.
        let (worker_tx, worker_rx) = mpsc::channel();
        pool.spawn_broadcast(move |context| {
            worker_tx
                .send((context.index(), thread::current().id()))
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut workers = BTreeMap::new();
        while workers.len() < 3 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match worker_rx.recv_timeout(remaining) {
                Ok((index, id)) => {
                    workers.insert(index, id);
                },
                Err(_) => break,
            }
        }
        assert_eq!(
            workers.len(),
            3,
            "all configured style workers must remain live after the creator exits; observed {workers:?}"
        );
        assert_eq!(workers.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2]);
        let worker_ids: HashSet<_> = workers.into_values().collect();
        assert_eq!(worker_ids.len(), 3);
        assert!(!worker_ids.contains(&creator_id));

        // Pool drop requests asynchronous termination. Only join handles already
        // observed finished, so cleanup is bounded even if shutdown regresses.
        drop(pool);
        let owned_handles = std::mem::take(&mut *handles.lock());
        assert_eq!(owned_handles.len(), 3);
        for handle in owned_handles {
            join_bounded(handle);
        }
    }
}
