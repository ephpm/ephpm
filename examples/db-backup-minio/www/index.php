<?php
// Minimal landing page for the backup demo. The embedded database is seeded
// offline (see compose.yaml) and snapshotted to MinIO every minute by ePHPm's
// [db.sqlite.backup] loop — this page just confirms the server is up.
header('Content-Type: text/plain');
echo "ePHPm db-backup demo\n";
echo "The embedded database at /app/data/app.db is backed up to MinIO every minute.\n";
echo "MinIO console: http://localhost:9001  (minioadmin / minioadmin)\n";
