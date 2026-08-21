const BUILD_ID_LEN: usize = 12;

const DEV_BUILD_ID: &str = "devbuild0000";

fn build_id() -> String {
    println!("cargo:rerun-if-env-changed=ABGEN_BUILD_ID");
    let given = std::env::var("ABGEN_BUILD_ID").unwrap_or_default();
    if given.is_empty() {
        if std::env::var_os("ABGEN_GIT_COMMIT").is_some() {
            println!(
                "cargo:warning=ABGEN_GIT_COMMIT is set but no longer used; set ABGEN_BUILD_ID \
                 instead (this build is stamped {DEV_BUILD_ID})"
            );
        }
        return DEV_BUILD_ID.to_string();
    }
    let hex = given
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    assert!(
        given.len() == BUILD_ID_LEN && hex,
        "ABGEN_BUILD_ID must be exactly {BUILD_ID_LEN} lowercase hex characters, got {given:?}. \
         It is a content id over the source tree (`nix eval --raw .#srcId`, or .#nixId for nix legs), never a git rev; \
         a variable-width value reintroduces the cross-commit drift this replaced."
    );
    given
}

fn main() {
    println!("cargo:rerun-if-env-changed=ABGEN_TURBOJPEG_LIB");

    let linux_gnu = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    if linux_gnu {
        println!("cargo:rerun-if-changed=compat/isoc23_shim.c");
        cc::Build::new()
            .file("compat/isoc23_shim.c")
            .link_lib_modifier("+whole-archive")
            .compile("abgen_isoc23_shim");
    }

    println!("cargo:rustc-check-cfg=cfg(abgen_static_turbojpeg)");
    println!("cargo:rerun-if-env-changed=ABGEN_TURBOJPEG_STATIC_DIR");
    if let Ok(dir) = std::env::var("ABGEN_TURBOJPEG_STATIC_DIR") {
        if !dir.is_empty() {
            println!("cargo:rustc-link-search=native={dir}");
            println!("cargo:rustc-link-lib=static=turbojpeg_iso");
            println!("cargo:rustc-cfg=abgen_static_turbojpeg");
        }
    }

    println!("cargo:rustc-env=ABGEN_BUILD_ID={}", build_id());
}
