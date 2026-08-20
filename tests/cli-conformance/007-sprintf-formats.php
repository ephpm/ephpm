<?php
printf("[%d] [%5d] [%-5d] [%05d] [%+d] [%+d]\n", 42, 42, 42, 42, 42, -42);
printf("[%s] [%10s] [%-10s] [%'x10s] [%.3s]\n", "hi", "hi", "hi", "hi", "hello");
printf("[%f] [%.2f] [%10.3f] [%-10.3f] [%e] [%.1E] [%g] [%G]\n",
    3.14159, 3.14159, 3.14159, 3.14159, 31415.9265, 31415.9265, 0.00001234, 12345678901.0);
printf("[%x] [%X] [%o] [%b] [%c] [%u]\n", 255, 255, 8, 5, 65, -1);
printf("[%'*12.4f] [%%]\n", 3.14159);
echo sprintf("%1\$s %2\$s %1\$s", "a", "b"), "\n";
echo number_format(1234567.891), "\n";
echo number_format(1234567.891, 2), "\n";
echo number_format(1234567.891, 2, ',', '.'), "\n";
echo number_format(-0.5), " ", number_format(0.5), " ", number_format(1.5), "\n";
vprintf("%s=%d\n", ["k", 7]);
printf("%d %d %d\n", "12", "12.9", true);
printf("%s\n", 1.5e-8);
