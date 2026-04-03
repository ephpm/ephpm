#!/bin/bash
# Setup a Symfony API skeleton inside the smoke test container.
#
# Uses composer to create a Symfony project and configures it to use
# Doctrine DBAL with pdo_mysql pointing at litewire (127.0.0.1:3306 -> SQLite).
#
# This script runs BEFORE ephpm starts. Migrations run in the smoke test
# after ephpm is healthy.

set -euo pipefail

APP_DIR="/var/www/html"

echo "==> Setting up Symfony"

# Install composer if not present
if [ ! -f /usr/local/bin/composer ]; then
    echo "==> Downloading Composer"
    curl -sSL https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer 2>/dev/null
fi

# Create Symfony project if not already present
if [ ! -f "${APP_DIR}/bin/console" ]; then
    echo "==> Creating Symfony API project"
    rm -rf /tmp/symfony-new
    composer create-project --prefer-dist --no-interaction --quiet \
        symfony/skeleton /tmp/symfony-new

    cp -a /tmp/symfony-new/. "${APP_DIR}/"
    rm -rf /tmp/symfony-new

    cd "${APP_DIR}"

    # Install API essentials
    composer require --no-interaction --quiet \
        doctrine/orm \
        doctrine/doctrine-bundle \
        doctrine/doctrine-migrations-bundle \
        symfony/maker-bundle --dev 2>/dev/null || true
fi

cd "${APP_DIR}"

# Configure .env for litewire SQLite via pdo_mysql (Doctrine DBAL)
cat > "${APP_DIR}/.env.local" << 'DOTENV'
APP_ENV=dev
APP_DEBUG=1
DATABASE_URL="mysql://root:@127.0.0.1:3306/symfony?serverVersion=8.0&charset=utf8mb4"
DOTENV

# Create a simple health-check controller
mkdir -p "${APP_DIR}/src/Controller"
cat > "${APP_DIR}/src/Controller/SmokeController.php" << 'CONTROLLER'
<?php

namespace App\Controller;

use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\Routing\Attribute\Route;

class SmokeController
{
    #[Route('/api/smoke', name: 'smoke', methods: ['GET'])]
    public function smoke(): JsonResponse
    {
        return new JsonResponse([
            'status' => 'ok',
            'framework' => 'Symfony',
            'php_version' => PHP_VERSION,
        ]);
    }

    #[Route('/api/db-check', name: 'db_check', methods: ['GET'])]
    public function dbCheck(): JsonResponse
    {
        try {
            // Simple connectivity test via PDO
            $pdo = new \PDO('mysql:host=127.0.0.1;port=3306;dbname=symfony', 'root', '');
            $stmt = $pdo->query('SELECT 1 as alive');
            $row = $stmt->fetch(\PDO::FETCH_ASSOC);
            return new JsonResponse([
                'status' => 'ok',
                'alive' => (int)$row['alive'],
            ]);
        } catch (\Exception $e) {
            return new JsonResponse([
                'status' => 'error',
                'message' => $e->getMessage(),
            ], 500);
        }
    }
}
CONTROLLER

# Clear Symfony cache to pick up the new controller
php "${APP_DIR}/bin/console" cache:clear --no-warmup --quiet 2>/dev/null || true

echo "==> Symfony files ready at ${APP_DIR}"
