<?php
// Placeholder page for the published ephpm/ephpm Docker image.
// Mount your application over /var/www/html to replace it.
header('Content-Type: text/html; charset=utf-8');
?>
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>ePHPm</title>
</head>
<body>
<h1>ePHPm is running</h1>
<p>Mount your site at <code>/var/www/html</code> and your configuration at
<code>/etc/ephpm/ephpm.toml</code> to replace this page.</p>
<p>Documentation: <a href="https://ephpm.dev/getting-started/install/#docker">ephpm.dev</a></p>
</body>
</html>
