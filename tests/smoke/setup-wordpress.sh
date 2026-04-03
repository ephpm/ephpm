#!/bin/bash
# Setup WordPress inside the smoke test container.
#
# Downloads WordPress + WP-CLI, configures wp-config.php to use pdo_mysql
# pointing at litewire's MySQL frontend (127.0.0.1:3306 -> SQLite).
#
# This script runs BEFORE ephpm starts. It only needs to lay down the files
# and wp-config.php. The actual WP install (creating tables) happens in the
# smoke test script after ephpm is healthy, since it needs the DB proxy up.

set -euo pipefail

WP_DIR="/var/www/html"
WP_VERSION="6.7"

echo "==> Setting up WordPress ${WP_VERSION}"

# Download WordPress if not already present
if [ ! -f "${WP_DIR}/wp-includes/version.php" ]; then
    echo "==> Downloading WordPress"
    curl -sSL "https://wordpress.org/wordpress-${WP_VERSION}.tar.gz" | tar xz --strip-components=1 -C "${WP_DIR}"
fi

# Download WP-CLI
if [ ! -f /usr/local/bin/wp ]; then
    echo "==> Downloading WP-CLI"
    curl -sSL https://raw.githubusercontent.com/wp-cli/builds/gh-pages/phar/wp-cli.phar -o /usr/local/bin/wp
    chmod +x /usr/local/bin/wp
fi

# Write wp-config.php pointing at litewire's MySQL frontend.
# litewire translates MySQL wire protocol to SQLite, so WordPress thinks
# it's talking to a real MySQL server.
cat > "${WP_DIR}/wp-config.php" << 'WPCONFIG'
<?php
define('DB_NAME',     'wordpress');
define('DB_USER',     'root');
define('DB_PASSWORD', '');
define('DB_HOST',     '127.0.0.1:3306');
define('DB_CHARSET',  'utf8mb4');
define('DB_COLLATE',  '');

// litewire/SQLite compatibility: MySQL-strict mode off
define('WP_DEBUG',       true);
define('WP_DEBUG_LOG',   '/tmp/wp-debug.log');
define('WP_DEBUG_DISPLAY', true);

// Authentication keys (static for test reproducibility)
define('AUTH_KEY',         'smoke-test-key-1');
define('SECURE_AUTH_KEY',  'smoke-test-key-2');
define('LOGGED_IN_KEY',   'smoke-test-key-3');
define('NONCE_KEY',        'smoke-test-key-4');
define('AUTH_SALT',        'smoke-test-salt-1');
define('SECURE_AUTH_SALT', 'smoke-test-salt-2');
define('LOGGED_IN_SALT',  'smoke-test-salt-3');
define('NONCE_SALT',       'smoke-test-salt-4');

$table_prefix = 'wp_';

if (!defined('ABSPATH')) {
    define('ABSPATH', __DIR__ . '/');
}

require_once ABSPATH . 'wp-settings.php';
WPCONFIG

echo "==> WordPress files ready at ${WP_DIR}"
