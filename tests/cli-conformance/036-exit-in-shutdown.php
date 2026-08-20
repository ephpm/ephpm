<?php
register_shutdown_function(function () {
    echo "in shutdown\n";
    exit(7);
});
echo "main\n";
exit(1);
