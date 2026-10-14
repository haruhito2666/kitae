//! Generate `testdata/fix_text_locked.qtsq` — a password-encrypted UTF-8
//! text fixture for the viewer's password-prompt flow. The password is a
//! placeholder ("hunter2", committed in tests); no real credentials here.
//!
//! Usage: cargo run -p kitae_qtsq --example make_locked_fixture
//!
//! Writes only inside the crate (hardcoded CARGO_MANIFEST_DIR/testdata path;
//! no arguments on purpose so the output location can't be redirected).

use std::path::PathBuf;

#[repr(C)]
struct QtsqContextOpaque {
    _private: [u8; 0],
}
type QtsqContext = QtsqContextOpaque;

unsafe extern "C" {
    fn kitae_qtsq_context_new(out: *mut *mut QtsqContext) -> i32;
    fn kitae_qtsq_context_free(ctx: *mut QtsqContext);
    // The vendored archive exports the typed helpers (qtsq_compress_text) but
    // not the generic qtsq_compress entry point, so encode text via the
    // UTF-8 text coder directly.
    fn qtsq_compress_text(ctx: *mut QtsqContext, text: *const i8, length: usize) -> i32;
    fn qtsq_vault_encrypt_password(ctx: *mut QtsqContext, password: *const i8) -> i32;
    fn qtsq_write(ctx: *mut QtsqContext, filename: *const i8) -> i32;
}

const QTSQ_OK: i32 = 0;
const PASSWORD: &str = "hunter2";
const PAYLOAD: &str = "Secret words behind the password. Only the right phrase opens this vault.";

fn main() {
    // Touch the library itself so the kitae_qtsq crate (and its linked C
    // shim) join the example's link — a bin that references only raw externs
    // otherwise pulls nothing from the crate's object set.
    let _ = kitae_qtsq::probe(&[]);
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("fix_text_locked.qtsq");
    let out_c = out_path
        .to_str()
        .expect("testdata path is valid UTF-8")
        .as_bytes()
        .to_vec();

    unsafe {
        let mut ctx: *mut QtsqContext = std::ptr::null_mut();
        let rc = kitae_qtsq_context_new(&mut ctx);
        assert_eq!(rc, QTSQ_OK, "qtsq_init failed: {rc}");

        let text_c: Vec<i8> = PAYLOAD.bytes().map(|b| b as i8).collect();
        let rc = qtsq_compress_text(ctx, text_c.as_ptr(), PAYLOAD.len());
        assert_eq!(rc, QTSQ_OK, "qtsq_compress_text failed: {rc}");

        // Encrypts with HKDF-SHA3 -> AES256 and sanitizes the context
        // (names move inside the envelope) as part of the vault flow.
        let pw_c: Vec<i8> = PASSWORD.bytes().map(|b| b as i8).chain([0]).collect();
        let rc = qtsq_vault_encrypt_password(ctx, pw_c.as_ptr());
        assert_eq!(rc, QTSQ_OK, "qtsq_vault_encrypt_password failed: {rc}");

        let out_c: Vec<i8> = out_c.into_iter().map(|b| b as i8).chain([0]).collect();
        let rc = qtsq_write(ctx, out_c.as_ptr());
        assert_eq!(rc, QTSQ_OK, "qtsq_write failed: {rc}");

        kitae_qtsq_context_free(ctx);
    }
    println!("wrote {}", out_path.display());
}
