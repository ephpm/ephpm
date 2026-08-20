<?php
var_dump(null, true, false, 0, 42, -42, PHP_INT_MAX, PHP_INT_MIN);
var_dump(0.0, -0.0, 1.5, -1.5, 1.0E308, PHP_FLOAT_EPSILON, PHP_FLOAT_MAX, PHP_FLOAT_MIN);
var_dump('', "a", "multi\nline", "utf-8 \u{2192} \u{FC}n\u{EF}code", "nul\0byte");
