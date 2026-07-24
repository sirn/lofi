import sys
f = open('lofi-ui/src/tui/mod.rs').read()
old = "    // The event log was only needed to rebuild the view and history; free it\n    // now so a large transcript isn't held for the session's lifetime.\n    drop(events);"
new = "    // Derive compaction state from the transcript so auto-compact's\n    // hysteresis and cooldown work immediately on resume, and the context\n    // gauge shows \"c\" when the session was compacted but not yet continued.\n    app.restore_compaction_state(&events);\n    // The event log was only needed to rebuild the view and history; free it\n    // now so a large transcript isn't held for the session's lifetime.\n    drop(events);"
assert old in f, 'old text not found'
f = f.replace(old, new, 1)
open('lofi-ui/src/tui/mod.rs', 'w').write(f)
print('done')
