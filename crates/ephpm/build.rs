use std::env;

fn main() {
    // When PHP is linked (release builds), override PHP's zend_signal_*
    // functions with our no-op wrappers in ephpm_wrapper.c.
    //
    // PHP's signal handling installs process-wide SIGPROF handlers that
    // crash when delivered to non-PHP threads (tokio workers). The --wrap
    // linker flag redirects all calls to these functions to our __wrap_
    // versions, which are no-ops.
    //
    // This must be in the binary crate's build.rs because rustc-link-arg
    // only takes effect for binary/cdylib targets, not library crates.
    if env::var_os("PHP_SDK_PATH").is_some() {
        for func in &[
            "zend_signal_startup",
            "zend_signal_init",
            "zend_signal_deactivate",
            "zend_signal_activate",
            "zend_signal_handler_unblock",
            // zend_set_timeout directly calls sigaction(SIGPROF) +
            // setitimer(ITIMER_PROF), bypassing the zend_signal system.
            // Must also be wrapped to prevent SIGPROF on worker threads.
            "zend_set_timeout",
            "zend_unset_timeout",
            // zend_call_stack_init probes stack boundaries on each request
            // startup. Fails on tokio spawn_blocking threads with small/
            // non-standard stacks. We disable stack checking anyway.
            "zend_call_stack_init",
        ] {
            println!("cargo::rustc-link-arg=-Wl,--wrap={func}");
        }
    }
}
