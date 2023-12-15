// check-fail

macro_rules! macro_rules { () => {} } //~ ERROR: user-defined macro may not be named `macro_rules`

macro_rules! {} //~ ERROR: expected identifier, found `{`
