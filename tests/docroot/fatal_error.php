<?php
// Intentionally calls an undefined function — triggers a PHP fatal error.
// Server must return 500 without hanging or crashing.
call_undefined_function_that_does_not_exist();
echo "should not reach here";
