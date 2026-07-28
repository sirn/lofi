function flush(    text,k) {
  if (n == 0) return
  text=""
  for (k=1;k<=n;k++) text=text block[k] "\n"
  if (text ~ /(# Errors|# Panics|# Safety|SAFETY|because|otherwise|instead|rather than|cannot|can't|must not|must remain|must be|invariant|constraint|compatib|workaround|quirk|race|security|soundness|unsound|deadlock|overflow|exhaust|unbounded|path traversal|symlink|upstream|regression|ambiguous|lossy|atomic|TOCTOU|backpressure|protocol|wire format|data loss|boundary|budget|limit|prevent|avoid|protect|preserv|guarantee|no safe|not safe|do not|never|canonical|durable|ordering|concurrent|legacy|backward|UTF-8|byte offset|root-to-leaf|append-only|crash|corrupt)/) {
    for (k=1;k<=n;k++) print block[k]
  }
  delete block; n=0
}
/^[[:space:]]*\/\// { block[++n]=$0; next }
{ flush(); print }
END { flush() }
