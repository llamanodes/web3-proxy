# deadlock

goerli_1    | 1 deadlocks detected
goerli_1    | Deadlock #0
goerli_1    | Thread Id 139881298757376
goerli_1    |    0:     0x5608f7f762d9 - backtrace::backtrace::trace::hbe74611947a262af
goerli_1    |    1:     0x5608f7f7a967 - backtrace::capture::Backtrace::new::h667fe9ee7ec04c33
goerli_1    |    2:     0x5608f7f6ed33 - parking_lot_core::parking_lot::deadlock_impl::on_unpark::h78879313dd6461e5
goerli_1    |    3:     0x5608f730dcd4 - parking_lot::raw_mutex::RawMutex::lock_slow::h9c58bf1ec322b8f6
goerli_1    |    4:     0x5608f78f2e87 - <moka::common::concurrent::housekeeper::ThreadPoolHousekeeper<T> as core::ops::drop::Drop>::drop::h4887dbe8ef7d7472
goerli_1    |    5:     0x5608f7909362 - alloc::sync::Arc<T>::drop_slow::h3de3d854b76812ea
goerli_1    |    6:     0x5608f7919596 - core::ptr::drop_in_place<moka::future::cache::Cache<web3_proxy::app_stats::UserProxyResponseKey,alloc::sync::Arc<web3_proxy::app_stats::ProxyResponseAggregate>,ahash::random_state::RandomState>>::h1bf4d8ebf87406ed
goerli_1    |    7:     0x5608f791ac00 - triomphe::arc::Arc<T>::drop_slow::h246e78aee1f2a265
goerli_1    |    8:     0x5608f78e38bd - crossbeam_epoch::deferred::Deferred::new::call::h395b93588d5e21a9
goerli_1    |    9:     0x5608f72fbaa2 - crossbeam_epoch::internal::Global::collect::h77479fc8b8898340
goerli_1    |   10:     0x5608f73ef22c - <moka::sync_base::base_cache::Inner<K,V,S> as moka::common::concurrent::housekeeper::InnerSync>::sync::h07f3f4f6db1c2598
goerli_1    |   11:     0x5608f75e4ee3 - moka::common::concurrent::housekeeper::ThreadPoolHousekeeper<T>::call_sync::h11b70044870c94f4
goerli_1    |   12:     0x5608f75e4b03 - moka::common::concurrent::housekeeper::ThreadPoolHousekeeper<T>::start_periodical_sync_job::{{closure}}::hdc1c253b1b156548
goerli_1    |   13:     0x5608f7cc8d15 - scheduled_thread_pool::Worker::run_job::hb3ae60b61103071b
goerli_1    |   14:     0x5608f7cc8b8b - scheduled_thread_pool::Worker::run::h760e10fe3281c379
goerli_1    |   15:     0x5608f7ccb294 - std::sys_common::backtrace::__rust_begin_short_backtrace::hc3b55a28c2ef3a5f
goerli_1    |   16:     0x5608f7cc9cb5 - core::ops::function::FnOnce::call_once{{vtable.shim}}::hf330c4157d74cf0e
goerli_1    |   17:     0x5608f7fc8dd3 - <alloc::boxed::Box<F,A> as core::ops::function::FnOnce<Args>>::call_once::h56d5fc072706762b
goerli_1    |                                at /rustc/a55dd71d5fb0ec5a6a3a9e8c27b2127ba491ce52/library/alloc/src/boxed.rs:1935:9
goerli_1    |                            <alloc::boxed::Box<F,A> as core::ops::function::FnOnce<Args>>::call_once::h41deef8e33b824bb
goerli_1    |                                at /rustc/a55dd71d5fb0ec5a6a3a9e8c27b2127ba491ce52/library/alloc/src/boxed.rs:1935:9
goerli_1    |                            std::sys::unix::thread::Thread::new::thread_start::ha6436304a1170bba
goerli_1    |                                at /rustc/a55dd71d5fb0ec5a6a3a9e8c27b2127ba491ce52/library/std/src/sys/unix/thread.rs:108:17
goerli_1    |   18:     0x7f3b309e9ea7 - start_thread
goerli_1    |   19:     0x7f3b307bfaef - clone
goerli_1    |   20:                0x0 - <unknown>
goerli_1    | 

also saw deadlocks on other chains (arbitrum, goerli, gnosis, optimism, polygon, fantom). though luckily not on eth. and it seems like it kept going.
i'm going to guess that the problem is nested caches.
refactor to maybe use a dashmap at one level? or flatten into one level and use channels more
