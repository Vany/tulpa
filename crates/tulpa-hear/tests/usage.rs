//! The binary's usage errors: anything but `--probe-mic <1..3600>` or `--probe-tap <1..3600>` exits 2 naming both.

use std::process::Command;

#[test]
fn bad_args_exit_2() {
    let bin = env!("CARGO_BIN_EXE_tulpa-hear");
    for bad in [
        &["--probe-tap"][..],
        &["--probe-tap", "0"],
        &["--probe-tap", "soon"],
        &["--probe-mic", "3601"],
        &["--nope", "5"],
        &[],
    ] {
        let out = Command::new(bin).args(bad).output();
        let code = out.as_ref().ok().and_then(|o| o.status.code());
        assert_eq!(code, Some(2), "{bad:?}");
        let err = out.map(|o| String::from_utf8_lossy(&o.stderr).into_owned());
        assert!(
            err.as_ref()
                .is_ok_and(|e| e.contains("--probe-tap") && e.contains("--probe-mic")),
            "{err:?}"
        );
    }
}
