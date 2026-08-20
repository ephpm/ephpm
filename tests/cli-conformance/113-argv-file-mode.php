<?php
var_dump($argv, $argc);
foreach (['argv', 'argc', 'PHP_SELF', 'SCRIPT_NAME', 'SCRIPT_FILENAME', 'PATH_TRANSLATED'] as $k) {
    echo $k, "=";
    var_dump($_SERVER[$k] ?? null);
}
