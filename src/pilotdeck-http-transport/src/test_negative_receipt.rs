//! #946: failure receipts are emitted only by explicitly bound assertions.
//! Normal test runs have no receipt environment and retain normal assertions.
use std::{fs::OpenOptions, io::Write};

pub(crate) const CONFIG_ENV: &str = "PDRS_NEGATIVE_RECEIPTS";
const PARENT_ENV: &str = "PDRS_NEGATIVE_PARENT_TEST";
pub(crate) fn record_failure(assertion: &str) {
    let Some(config) = std::env::var_os(CONFIG_ENV) else {
        return;
    };
    let config = config.to_str().expect("#946 receipt config UTF-8");
    let run: serde_json::Value = serde_json::from_str(config).expect("#946 receipt config JSON");
    let nonce = run["nonce"].as_str().expect("#946 string nonce");
    assert!(
        nonce.len() == 64 && nonce.bytes().all(|b| b.is_ascii_hexdigit()),
        "#946 invalid receipt nonce"
    );
    let thread = std::thread::current();
    let parent = std::env::var(PARENT_ENV).ok();
    let test = parent
        .as_deref()
        .or_else(|| thread.name())
        .expect("#946 named failure test");
    let targets: Vec<_> = run["targets"]
        .as_array()
        .expect("#946 targets array")
        .iter()
        .filter(|target| target["test"] == test && target["assertion"] == assertion)
        .collect();
    assert!(targets.len() <= 1, "#946 duplicate receipt target");
    if let Some(target) = targets.first() {
        let path = std::path::Path::new(target["path"].as_str().expect("#946 receipt path string"));
        assert!(path.is_absolute(), "#946 absolute receipt path required");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("#946 create fresh failure receipt");
        file.write_all(nonce.as_bytes())
            .expect("#946 write failure receipt");
        file.sync_all().expect("#946 flush failure receipt");
    }
}

#[macro_export]
macro_rules! negative_assert_eq {
    ($left:expr, $right:expr, $($message:tt)+) => {
        match (&$left, &$right) {
            (left, right) => {
                if left != right {
                    let assertion = format!($($message)+);
                    $crate::test_negative_receipt::record_failure(&assertion);
                    assert_eq!(left, right, "{assertion}");
                }
            }
        }
    };
}

#[macro_export]
macro_rules! negative_assert {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            let assertion = format!($($message)+);
            $crate::test_negative_receipt::record_failure(&assertion);
            panic!("{assertion}");
        }
    };
}
