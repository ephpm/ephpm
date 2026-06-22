<?php
/**
 * WordPress config for the ePHPm embedded-object-cache demo.
 *
 * Database goes through ePHPm's MySQL proxy (127.0.0.1:3306 -> mysql:3306).
 * The object cache is the ephpm/cache-wordpress drop-in (installed to
 * wp-content/object-cache.php by the init container), which talks straight
 * to ePHPm's in-process KV store via the ephpm_kv_* SAPI functions.
 */

// -- Database: ePHPm MySQL proxy (forwards to the external mysql container) --
define( 'DB_NAME',     'wordpress' );
define( 'DB_USER',     'wordpress' );
define( 'DB_PASSWORD', 'wordpress' );
define( 'DB_HOST',     '127.0.0.1' );   // ePHPm proxy listener, same container
define( 'DB_CHARSET',  'utf8mb4' );
define( 'DB_COLLATE',  '' );

// -- Object cache: enable WordPress's persistent object cache --
//    The drop-in (wp-content/object-cache.php) is what makes this persistent;
//    WP_CACHE just opts in.
define( 'WP_CACHE', true );

// -- Auth keys: replace with values from --
//    https://api.wordpress.org/secret-key/1.1/salt/ before any real use.
define( 'AUTH_KEY',         'change-me' );
define( 'SECURE_AUTH_KEY',  'change-me' );
define( 'LOGGED_IN_KEY',    'change-me' );
define( 'NONCE_KEY',        'change-me' );
define( 'AUTH_SALT',        'change-me' );
define( 'SECURE_AUTH_SALT', 'change-me' );
define( 'LOGGED_IN_SALT',   'change-me' );
define( 'NONCE_SALT',       'change-me' );

$table_prefix = 'wp_';

if ( ! defined( 'ABSPATH' ) ) {
    define( 'ABSPATH', __DIR__ . '/' );
}
require_once ABSPATH . 'wp-settings.php';
