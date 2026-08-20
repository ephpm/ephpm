<?php
var_dump(explode(",", "a,,b", -1), explode(",", "a,b,c", 2), explode(",", "solo"));
var_dump(implode(",", [1, null, true, 2.5]), join("|", ["a"]));
$count = 0;
var_dump(str_replace(["a", "b"], ["b", "c"], "aabb", $count), $count);
var_dump(str_ireplace("A", "x", "aAa"));
var_dump(substr_replace("Hello", "world", 0, 0), substr_replace("Hello", "X", -3, 1));
var_dump(substr_count("hello hello", "ll"), str_word_count("Fred's flat, one two"));
var_dump(sprintf("%08.3f", -3.2), vsprintf("%s-%s", ["a", "b"]));
var_dump(strcmp("a", "b") < 0, strcmp("b", "a") > 0, strcmp("a", "a"), strcasecmp("A", "a"), strncmp("abc", "abd", 2), strnatcmp("img10", "img2"));
var_dump(sscanf("age: 25 name: Bob", "age: %d name: %s"));
var_dump(strtok("a b,c", " ,"), strtok(" ,"), strtok(" ,"));
var_dump(str_starts_with("hello", "he"), str_ends_with("hello", "lo"), str_contains("hello", ""));
var_dump(strspn("42 is the answer", "1234567890"), strcspn("abcd4", "04"));
var_dump(strpbrk("This is a test", "st"), strstr("user@host", "@"), strstr("user@host", "@", true), strrchr("a/b/c", "/"));
var_dump(wordwrap("A very long woooooooooooord.", 8, "\n", true));
var_dump(str_word_count("Hello fri3nd, you're looking good", 2));
