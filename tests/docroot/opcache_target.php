<?php
// Trivial target file whose bytecode is loaded into OPcache by
// opcache_status.php. The invalidation e2e test uses this file's cached
// status as a proxy for "did the watcher drop the docroot's bytecode?".

function ephpm_opcache_target_marker(): string {
    return 'ephpm-opcache-target';
}
