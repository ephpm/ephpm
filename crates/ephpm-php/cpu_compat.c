/*
 * Compatibility shim for GCC __cpu_indicator_init_local.
 *
 * static-php-cli builds libphp.a inside an Alpine container with GCC 14+,
 * whose __builtin_cpu_supports() / __builtin_cpu_init() expand to a call
 * to __cpu_indicator_init_local — a no-PLT, static-link-friendly variant
 * of __cpu_indicator_init introduced in GCC 14. Older host toolchains
 * (notably Ubuntu 24.04's GCC 13.2, which the ephpm CI image uses) ship
 * a libgcc.a that only provides __cpu_indicator_init.
 *
 * libphp.a's zend_jit_startup references __cpu_indicator_init_local from
 * its precompiled object code, so we can't change PHP's emitted symbol.
 * Define a thunk that forwards to __cpu_indicator_init — same semantics,
 * different linkage. Functionally a no-op once GCC's libgcc catches up.
 *
 * Compiled into the ephpm_wrapper static archive by build.rs's cc::Build
 * step.
 */

#if defined(__linux__) && defined(__GNUC__)

extern void __cpu_indicator_init(void);

void __cpu_indicator_init_local(void) {
    __cpu_indicator_init();
}

#endif
