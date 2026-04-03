/* PHP embed SAPI headers for bindgen.
 *
 * These are the headers needed to embed PHP in a Rust application.
 * bindgen processes this file to generate Rust FFI bindings.
 *
 * Requires PHP_SDK_PATH to be set, with headers at:
 *   $PHP_SDK_PATH/include/php/
 */

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
