<?php
// Generates a response with an ETag header based on content.
// Used by e2e tests for PHP ETag cache validation.
$content = "ETag test content - " . date('Y');
$etag = '"' . md5($content) . '"';

header("ETag: $etag");
header("Content-Type: text/plain");
echo $content;
