<?php
/**
 * WordPress config for the ePHPm external-services compose demo.
 *
 * Database goes through ePHPm's MySQL proxy (127.0.0.1:3306 -> mysql:3306).
 * Object cache goes to the standalone Redis container.
 */

// -- Database: ePHPm MySQL proxy (forwards to the external mysql container) --
define( 'DB_NAME',     'wordpress' );
define( 'DB_USER',     'wordpress' );
define( 'DB_PASSWORD', 'wordpress' );
define( 'DB_HOST',     '127.0.0.1' );   // ePHPm proxy listener, same container
define( 'DB_CHARSET',  'utf8mb4' );
define( 'DB_COLLATE',  '' );

// -- Object cache: external Redis container (NOT ePHPm's embedded KV) --
define( 'WP_REDIS_PLUGIN_PATH', '/app/wordpress/wp-content/plugins/redis-cache' );
define( 'WP_REDIS_HOST',  'redis' );
define( 'WP_REDIS_PORT',  6379 );
define( 'WP_REDIS_CLIENT', 'predis' );
define( 'WP_REDIS_TIMEOUT', 1 );
define( 'WP_REDIS_READ_TIMEOUT', 1 );
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
