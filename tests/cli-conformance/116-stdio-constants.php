<?php
fwrite(STDOUT, "to-stdout\n");
fwrite(STDERR, "to-stderr\n");
var_dump(get_resource_type(STDIN), get_resource_type(STDOUT));
$in = stream_get_contents(STDIN);
var_dump($in);
var_dump(feof(STDIN));
$meta = stream_get_meta_data(STDIN);
var_dump($meta['blocked'], $meta['seekable'], $meta['stream_type']);
