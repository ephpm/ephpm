/* PHP embed SAPI headers for bindgen.
 *
 * These are the headers needed to embed PHP in a Rust application.
 * bindgen processes this file to generate Rust FFI bindings.
 *
 * Requires PHP_SDK_PATH to be set, with headers at:
 *   $PHP_SDK_PATH/include/php/
 */

#ifdef _WIN32
/* PHP's Zend/zend_operators.h calls LongLongAdd / LongLongSub /
 * ULongLongAdd / ULongLongSub directly without declaring them. MSVC
 * cl.exe tolerates this as a warning because it auto-includes
 * intsafe.h transitively via the Windows SDK umbrella; clang
 * (libclang, used by bindgen) is strict and errors on the implicit
 * declaration. Pull intsafe.h in here so bindgen sees the prototypes
 * before parsing the Zend headers. */
#include <intsafe.h>
#endif

#include "sapi/embed/php_embed.h"
#include "main/php.h"
#include "main/SAPI.h"
#include "main/php_main.h"
#include "main/php_variables.h"
#include "Zend/zend.h"
#include "Zend/zend_API.h"
#include "Zend/zend_stream.h"
#ifdef ZTS
#include "TSRM/TSRM.h"
#endif
