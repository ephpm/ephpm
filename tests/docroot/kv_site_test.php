<?php
header('Content-Type: text/plain');

$site = $_GET['site'] ?? 'unknown';

ephpm_kv_set('isolation-key', "from-{$site}", 0);
echo ephpm_kv_get('isolation-key');
