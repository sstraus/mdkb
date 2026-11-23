//! The ONNX Runtime thread bound, observed as OS threads.
//!
//! Its own test binary on purpose: the ORT environment is process-global and
//! the oracle counts the threads of the whole process, so no other test may
//! share the process.

/// A valid ONNX model with one `Relu` over a `[1, 4]` float tensor. Generated
/// with the `onnx` Python package (`helper.make_model`, opset 13, IR 7) and
/// checked with `onnx.checker.check_model`.
const RELU_MODEL: [u8; 80] = [
    8, 7, 18, 9, 109, 100, 107, 98, 45, 116, 101, 115, 116, 58, 59, 10, 12, 10, 1, 120, 18, 1, 121,
    34, 4, 82, 101, 108, 117, 18, 1, 103, 90, 19, 10, 1, 120, 18, 14, 10, 12, 8, 1, 18, 8, 10, 2,
    8, 1, 10, 2, 8, 4, 98, 19, 10, 1, 121, 18, 14, 10, 12, 8, 1, 18, 8, 10, 2, 8, 1, 10, 2, 8, 4,
    66, 4, 10, 0, 16, 13,
];

#[cfg(target_os = "linux")]
fn os_threads() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

#[cfg(target_os = "macos")]
fn os_threads() -> usize {
    // SAFETY: PROC_PIDTASKINFO fills exactly one `proc_taskinfo` for this pid.
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        );
        assert_eq!(got, size, "proc_pidinfo(PROC_PIDTASKINFO)");
        info.pti_threadnum as usize
    }
}

/// Catches: a model session that builds its own intra-op pool with one thread
/// per core. fastembed builds every session with
/// `with_intra_threads(available_parallelism())`; measured 2026-11-12, the
/// reranker then held 9.2-11.1 cores per run and `mdkb embed` 7.4.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_session_built_like_fastembed_adds_at_most_the_bounded_threads() {
    let cores = std::thread::available_parallelism().unwrap().get();
    assert!(
        cores > mdkb::llm::DEFAULT_ORT_THREADS + 2,
        "{cores} cores cannot tell a per-core pool from the bounded one"
    );
    let before = os_threads();

    mdkb::llm::bound_onnx_threads();
    let session = ort::session::Session::builder()
        .unwrap()
        .with_intra_threads(cores)
        .unwrap()
        .commit_from_memory(&RELU_MODEL)
        .unwrap();
    let input =
        ort::value::Tensor::from_array(([1usize, 4], vec![-1.0f32, 2.0, -3.0, 4.0])).unwrap();
    let outputs = session.run(ort::inputs!["x" => input].unwrap()).unwrap();
    let (_, y) = outputs["y"].try_extract_raw_tensor::<f32>().unwrap();
    assert_eq!(y, [0.0, 2.0, 0.0, 4.0], "the bounded session still runs");

    let added = os_threads() - before;
    assert!(
        added <= mdkb::llm::DEFAULT_ORT_THREADS + 1,
        "ONNX Runtime added {added} threads on {cores} cores; bound is {}",
        mdkb::llm::DEFAULT_ORT_THREADS
    );
}
