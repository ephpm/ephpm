#!/bin/bash
# Setup a fresh Laravel project inside the smoke test container.
#
# Uses composer to create a Laravel project and configures it to use
# pdo_mysql pointing at litewire's MySQL frontend (127.0.0.1:3306 -> SQLite).
#
# This script runs BEFORE ephpm starts. Migrations happen in the smoke test
# after ephpm is healthy (since the DB proxy must be running).

set -euo pipefail

APP_DIR="/var/www/html"

echo "==> Setting up Laravel"

# Install composer if not present
if [ ! -f /usr/local/bin/composer ]; then
    echo "==> Downloading Composer"
    curl -sSL https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer 2>/dev/null
fi

# Create Laravel project if not already present
if [ ! -f "${APP_DIR}/artisan" ]; then
    echo "==> Creating Laravel project"
    # Create in a temp dir then move, since composer create-project needs an empty dir
    rm -rf /tmp/laravel-new
    composer create-project --prefer-dist --no-interaction --quiet \
        laravel/laravel /tmp/laravel-new

    # Move into place
    cp -a /tmp/laravel-new/. "${APP_DIR}/"
    rm -rf /tmp/laravel-new
fi

# Configure .env for litewire SQLite via pdo_mysql
cat > "${APP_DIR}/.env" << 'DOTENV'
APP_NAME=EphpmSmokeTest
APP_ENV=testing
APP_KEY=base64:dGhpcyBpcyBhIHNtb2tlIHRlc3Qga2V5IGZvciBjaTEyMzQ=
APP_DEBUG=true
APP_URL=http://localhost:8080

LOG_CHANNEL=stderr
LOG_LEVEL=debug

DB_CONNECTION=mysql
DB_HOST=127.0.0.1
DB_PORT=3306
DB_DATABASE=laravel
DB_USERNAME=root
DB_PASSWORD=

SESSION_DRIVER=file
CACHE_STORE=file
QUEUE_CONNECTION=sync
DOTENV

# Create a simple API test route
cat > "${APP_DIR}/routes/api.php" << 'APIROUTE'
<?php

use Illuminate\Support\Facades\Route;

Route::get('/smoke', function () {
    return response()->json([
        'status' => 'ok',
        'framework' => 'Laravel',
        'php_version' => PHP_VERSION,
    ]);
});

Route::get('/db-check', function () {
    try {
        $tables = \Illuminate\Support\Facades\DB::select("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name");
        return response()->json([
            'status' => 'ok',
            'tables' => array_map(fn($t) => $t->name, $tables),
        ]);
    } catch (\Exception $e) {
        // litewire translates to SQLite but exposes MySQL wire protocol,
        // so SHOW TABLES may not work — fall back to a simpler check
        try {
            \Illuminate\Support\Facades\DB::select('SELECT 1 as alive');
            return response()->json(['status' => 'ok', 'tables' => ['db_reachable']]);
        } catch (\Exception $e2) {
            return response()->json(['status' => 'error', 'message' => $e2->getMessage()], 500);
        }
    }
});
APIROUTE

echo "==> Laravel files ready at ${APP_DIR}"
