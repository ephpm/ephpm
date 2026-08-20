<?php
// a comment that -w should strip

/* block
   comment */
function spaced( $x )   {
    return   $x + 1 ;  # hash comment
}

echo spaced( 1 ) ;
