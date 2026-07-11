/* Resolver compat shim.
 *
 * PHP's ext/standard/dns.c references the underscored resolver names
 * (`__dn_expand`, `__dn_skipname`, `__res_nsearch`) which glibc historically
 * exported as unversioned compatibility aliases alongside the public
 * `dn_expand` / `dn_skipname` / `res_nsearch`. Ubuntu 22.04+ (glibc 2.35+)
 * strips the unversioned aliases: only versioned `__dn_expand@GLIBC_2.2.5`
 * remains, and rust-lld / newer GNU ld will not silently match an
 * unversioned reference to a versioned definition.
 *
 * Rebuilding libphp.a inside the SDK against a newer glibc would fix it
 * upstream; until that lands, this shim provides the underscored names as
 * thin wrappers over the still-exported public entry points. Runtime cost
 * is a single tail call. No behavior change.
 */

#include <resolv.h>

int __dn_expand(const unsigned char *msg, const unsigned char *eomorig,
                const unsigned char *comp_dn, char *exp_dn, int length) {
    return dn_expand(msg, eomorig, comp_dn, exp_dn, length);
}

int __dn_skipname(const unsigned char *comp_dn, const unsigned char *eom) {
    return dn_skipname(comp_dn, eom);
}

int __res_nsearch(res_state statp, const char *dname, int class, int type,
                  unsigned char *answer, int anslen) {
    return res_nsearch(statp, dname, class, type, answer, anslen);
}
