//! Shim which is passed to Cargo as "rustc" when running the bootstrap.
//!
//! This shim will take care of some various tasks that our build process
//! requires that Cargo can't quite do through normal configuration:
//!
//! 1. When compiling build scripts and build dependencies, we need a guaranteed
//!    full standard library available. The only compiler which actually has
//!    this is the snapshot, so we detect this situation and always compile with
//!    the snapshot compiler.
//! 2. We pass a bunch of `--cfg` and other flags based on what we're compiling
//!    (and this slightly differs based on a whether we're using a snapshot or
//!    not), so we do that all here.
//!
//! This may one day be replaced by RUSTFLAGS, but the dynamic nature of
//! switching compilers for the bootstrap and for build scripts will probably
//! never get replaced.

// NO-RUSTC-WRAPPER
#![deny(warnings, rust_2018_idioms, unused_lifetimes)]

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::time::Instant;

fn main() {
    let mut args = env::args_os().skip(1).collect::<Vec<_>>();

    // Append metadata suffix for internal crates. See the corresponding entry
    // in bootstrap/lib.rs for details.
    if let Ok(s) = env::var("RUSTC_METADATA_SUFFIX") {
        for i in 1..args.len() {
            // Dirty code for borrowing issues
            let mut new = None;
            if let Some(current_as_str) = args[i].to_str() {
                if (&*args[i - 1] == "-C" && current_as_str.starts_with("metadata")) ||
                    current_as_str.starts_with("-Cmetadata") {
                    new = Some(format!("{}-{}", current_as_str, s));
                }
            }
            if let Some(new) = new { args[i] = new.into(); }
        }
    }

    // Detect whether or not we're a build script depending on whether --target
    // is passed (a bit janky...)
    let target = args.windows(2)
        .find(|w| &*w[0] == "--target")
        .and_then(|w| w[1].to_str());
    let version = args.iter().find(|w| &**w == "-vV");

    let verbose = match env::var("RUSTC_VERBOSE") {
        Ok(s) => usize::from_str(&s).expect("RUSTC_VERBOSE should be an integer"),
        Err(_) => 0,
    };

    // Use a different compiler for build scripts, since there may not yet be a
    // libstd for the real compiler to use. However, if Cargo is attempting to
    // determine the version of the compiler, the real compiler needs to be
    // used. Currently, these two states are differentiated based on whether
    // --target and -vV is/isn't passed.
    let (rustc, libdir) = if target.is_none() && version.is_none() {
        ("RUSTC_SNAPSHOT", "RUSTC_SNAPSHOT_LIBDIR")
    } else {
        ("RUSTC_REAL", "RUSTC_LIBDIR")
    };
    let stage = env::var("RUSTC_STAGE").expect("RUSTC_STAGE was not set");
    let sysroot = env::var_os("RUSTC_SYSROOT").expect("RUSTC_SYSROOT was not set");
    let on_fail = env::var_os("RUSTC_ON_FAIL").map(|of| Command::new(of));

    let rustc = env::var_os(rustc).unwrap_or_else(|| panic!("{:?} was not set", rustc));
    let libdir = env::var_os(libdir).unwrap_or_else(|| panic!("{:?} was not set", libdir));
    let mut dylib_path = bootstrap::util::dylib_path();
    dylib_path.insert(0, PathBuf::from(&libdir));

    let mut cmd = Command::new(rustc);
    cmd.args(&args)
        .env(bootstrap::util::dylib_path_var(),
             env::join_paths(&dylib_path).unwrap());

    // Get the name of the crate we're compiling, if any.
    let crate_name = args.windows(2)
        .find(|args| args[0] == "--crate-name")
        .and_then(|args| args[1].to_str());

    if let Some(crate_name) = crate_name {
        if let Some(target) = env::var_os("RUSTC_TIME") {
            if target == "all" ||
                target.into_string().unwrap().split(",").any(|c| c.trim() == crate_name)
            {
                cmd.arg("-Ztime");
            }
        }
    }

    // Non-zero stages must all be treated uniformly to avoid problems when attempting to uplift
    // compiler libraries and such from stage 1 to 2.
    //
    // FIXME: the fact that core here is excluded is due to core_arch from our stdarch submodule
    // being broken on the beta compiler with bootstrap passed, so this is a temporary workaround
    // (we've just snapped, so there are no cfg(bootstrap) related annotations in core).
    if stage == "0" {
        if crate_name != Some("core") {
            cmd.arg("--cfg").arg("bootstrap");
        } else {
            // NOTE(eddyb) see FIXME above, except now we need annotations again in core.
            cmd.arg("--cfg").arg("boostrap_stdarch_ignore_this");
        }
    }

    // Print backtrace in case of ICE
    if env::var("RUSTC_BACKTRACE_ON_ICE").is_ok() && env::var("RUST_BACKTRACE").is_err() {
        cmd.env("RUST_BACKTRACE", "1");
    }

    cmd.env("RUSTC_BREAK_ON_ICE", "1");

    if let Ok(debuginfo_level) = env::var("RUSTC_DEBUGINFO_LEVEL") {
        cmd.arg(format!("-Cdebuginfo={}", debuginfo_level));
    }

    if env::var_os("RUSTC_DENY_WARNINGS").is_some() &&
       env::var_os("RUSTC_EXTERNAL_TOOL").is_none() {
        // When extending this list, search for `NO-RUSTC-WRAPPER` and add the new lints
        // there as well, some code doesn't go through this `rustc` wrapper.
        cmd.arg("-Dwarnings");
        cmd.arg("-Drust_2018_idioms");
        cmd.arg("-Dunused_lifetimes");
        if use_internal_lints(crate_name) {
            cmd.arg("-Zunstable-options");
            cmd.arg("-Drustc::internal");
        }
    }

    if let Some(target) = target {
        // The stage0 compiler has a special sysroot distinct from what we
        // actually downloaded, so we just always pass the `--sysroot` option,
        // unless one is already set.
        if !args.iter().any(|arg| arg == "--sysroot") {
            cmd.arg("--sysroot").arg(&sysroot);
        }

        cmd.arg("-Zexternal-macro-backtrace");

        // Link crates to the proc macro crate for the target, but use a host proc macro crate
        // to actually run the macros
        if env::var_os("RUST_DUAL_PROC_MACROS").is_some() {
            cmd.arg("-Zdual-proc-macros");
        }

        // When we build Rust dylibs they're all intended for intermediate
        // usage, so make sure we pass the -Cprefer-dynamic flag instead of
        // linking all deps statically into the dylib.
        if env::var_os("RUSTC_NO_PREFER_DYNAMIC").is_none() {
            cmd.arg("-Cprefer-dynamic");
        }

        // Help the libc crate compile by assisting it in finding various
        // sysroot native libraries.
        if let Some(s) = env::var_os("MUSL_ROOT") {
            if target.contains("musl") {
                let mut root = OsString::from("native=");
                root.push(&s);
                root.push("/lib");
                cmd.arg("-L").arg(&root);
            }
        }
        if let Some(s) = env::var_os("WASI_ROOT") {
            let mut root = OsString::from("native=");
            root.push(&s);
            root.push("/lib/wasm32-wasi");
            cmd.arg("-L").arg(&root);
        }

        // Override linker if necessary.
        if let Ok(target_linker) = env::var("RUSTC_TARGET_LINKER") {
            cmd.arg(format!("-Clinker={}", target_linker));
        }

        // If we're compiling specifically the `panic_abort` crate then we pass
        // the `-C panic=abort` option. Note that we do not do this for any
        // other crate intentionally as this is the only crate for now that we
        // ship with panic=abort.
        //
        // This... is a bit of a hack how we detect this. Ideally this
        // information should be encoded in the crate I guess? Would likely
        // require an RFC amendment to RFC 1513, however.
        //
        // `compiler_builtins` are unconditionally compiled with panic=abort to
        // workaround undefined references to `rust_eh_unwind_resume` generated
        // otherwise, see issue https://github.com/rust-lang/rust/issues/43095.
        if crate_name == Some("panic_abort") ||
           crate_name == Some("compiler_builtins") && stage != "0" {
            cmd.arg("-C").arg("panic=abort");
        }

        // Set various options from config.toml to configure how we're building
        // code.
        let debug_assertions = match env::var("RUSTC_DEBUG_ASSERTIONS") {
            Ok(s) => if s == "true" { "y" } else { "n" },
            Err(..) => "n",
        };

        // The compiler builtins are pretty sensitive to symbols referenced in
        // libcore and such, so we never compile them with debug assertions.
        if crate_name == Some("compiler_builtins") {
            cmd.arg("-C").arg("debug-assertions=no");
        } else {
            cmd.arg("-C").arg(format!("debug-assertions={}", debug_assertions));
        }

        if let Ok(s) = env::var("RUSTC_CODEGEN_UNITS") {
            cmd.arg("-C").arg(format!("codegen-units={}", s));
        }

        // Emit save-analysis info.
        if env::var("RUSTC_SAVE_ANALYSIS") == Ok("api".to_string()) {
            cmd.arg("-Zsave-analysis");
            cmd.env("RUST_SAVE_ANALYSIS_CONFIG",
                    "{\"output_file\": null,\"full_docs\": false,\
                     \"pub_only\": true,\"reachable_only\": false,\
                     \"distro_crate\": true,\"signatures\": false,\"borrow_data\": false}");
        }

        // Dealing with rpath here is a little special, so let's go into some
        // detail. First off, `-rpath` is a linker option on Unix platforms
        // which adds to the runtime dynamic loader path when looking for
        // dynamic libraries. We use this by default on Unix platforms to ensure
        // that our nightlies behave the same on Windows, that is they work out
        // of the box. This can be disabled, of course, but basically that's why
        // we're gated on RUSTC_RPATH here.
        //
        // Ok, so the astute might be wondering "why isn't `-C rpath` used
        // here?" and that is indeed a good question to task. This codegen
        // option is the compiler's current interface to generating an rpath.
        // Unfortunately it doesn't quite suffice for us. The flag currently
        // takes no value as an argument, so the compiler calculates what it
        // should pass to the linker as `-rpath`. This unfortunately is based on
        // the **compile time** directory structure which when building with
        // Cargo will be very different than the runtime directory structure.
        //
        // All that's a really long winded way of saying that if we use
        // `-Crpath` then the executables generated have the wrong rpath of
        // something like `$ORIGIN/deps` when in fact the way we distribute
        // rustc requires the rpath to be `$ORIGIN/../lib`.
        //
        // So, all in all, to set up the correct rpath we pass the linker
        // argument manually via `-C link-args=-Wl,-rpath,...`. Plus isn't it
        // fun to pass a flag to a tool to pass a flag to pass a flag to a tool
        // to change a flag in a binary?
        if env::var("RUSTC_RPATH") == Ok("true".to_string()) {
            let rpath = if target.contains("apple") {

                // Note that we need to take one extra step on macOS to also pass
                // `-Wl,-instal_name,@rpath/...` to get things to work right. To
                // do that we pass a weird flag to the compiler to get it to do
                // so. Note that this is definitely a hack, and we should likely
                // flesh out rpath support more fully in the future.
                cmd.arg("-Z").arg("osx-rpath-install-name");
                Some("-Wl,-rpath,@loader_path/../lib")
            } else if !target.contains("windows") &&
                      !target.contains("wasm32") &&
                      !target.contains("fuchsia") {
                Some("-Wl,-rpath,$ORIGIN/../lib")
            } else {
                None
            };
            if let Some(rpath) = rpath {
                cmd.arg("-C").arg(format!("link-args={}", rpath));
            }
        }

        if let Ok(s) = env::var("RUSTC_CRT_STATIC") {
            if s == "true" {
                cmd.arg("-C").arg("target-feature=+crt-static");
            }
            if s == "false" {
                cmd.arg("-C").arg("target-feature=-crt-static");
            }
        }
    } else {
        // Override linker if necessary.
        if let Ok(host_linker) = env::var("RUSTC_HOST_LINKER") {
            cmd.arg(format!("-Clinker={}", host_linker));
        }

        if let Ok(s) = env::var("RUSTC_HOST_CRT_STATIC") {
            if s == "true" {
                cmd.arg("-C").arg("target-feature=+crt-static");
            }
            if s == "false" {
                cmd.arg("-C").arg("target-feature=-crt-static");
            }
        }
    }

    if let Ok(map) = env::var("RUSTC_DEBUGINFO_MAP") {
        cmd.arg("--remap-path-prefix").arg(&map);
    }

    // Force all crates compiled by this compiler to (a) be unstable and (b)
    // allow the `rustc_private` feature to link to other unstable crates
    // also in the sysroot. We also do this for host crates, since those
    // may be proc macros, in which case we might ship them.
    if env::var_os("RUSTC_FORCE_UNSTABLE").is_some() && (stage != "0" || target.is_some()) {
        cmd.arg("-Z").arg("force-unstable-if-unmarked");
    }

    if env::var_os("RUSTC_PARALLEL_COMPILER").is_some() {
        cmd.arg("--cfg").arg("parallel_compiler");
    }

    if verbose > 1 {
        eprintln!(
            "rustc command: {:?}={:?} {:?}",
            bootstrap::util::dylib_path_var(),
            env::join_paths(&dylib_path).unwrap(),
            cmd,
        );
        eprintln!("sysroot: {:?}", sysroot);
        eprintln!("libdir: {:?}", libdir);
    }

    if let Some(mut on_fail) = on_fail {
        let e = match cmd.status() {
            Ok(s) if s.success() => std::process::exit(0),
            e => e,
        };
        println!("\nDid not run successfully: {:?}\n{:?}\n-------------", e, cmd);
        exec_cmd(&mut on_fail).expect("could not run the backup command");
        std::process::exit(1);
    }

    if env::var_os("RUSTC_PRINT_STEP_TIMINGS").is_some() {
        if let Some(crate_name) = crate_name {
            let start = Instant::now();
            let status = cmd
                .status()
                .unwrap_or_else(|_| panic!("\n\n failed to run {:?}", cmd));
            let dur = start.elapsed();

            let is_test = args.iter().any(|a| a == "--test");
            eprintln!("[RUSTC-TIMING] {} test:{} {}.{:03}",
                      crate_name,
                      is_test,
                      dur.as_secs(),
                      dur.subsec_nanos() / 1_000_000);

            match status.code() {
                Some(i) => std::process::exit(i),
                None => {
                    eprintln!("rustc exited with {}", status);
                    std::process::exit(0xfe);
                }
            }
        }
    }

    let code = exec_cmd(&mut cmd).unwrap_or_else(|_| panic!("\n\n failed to run {:?}", cmd));
    std::process::exit(code);
}

// Rustc crates for which internal lints are in effect.
fn use_internal_lints(crate_name: Option<&str>) -> bool {
    crate_name.map_or(false, |crate_name| {
        crate_name.starts_with("rustc") || crate_name.starts_with("syntax") ||
        ["arena", "fmt_macros"].contains(&crate_name)
    })
}

#[cfg(unix)]
fn exec_cmd(cmd: &mut Command) -> io::Result<i32> {
    use std::os::unix::process::CommandExt;
    Err(cmd.exec())
}

#[cfg(not(unix))]
fn exec_cmd(cmd: &mut Command) -> io::Result<i32> {
    cmd.status().map(|status| status.code().unwrap())
}
