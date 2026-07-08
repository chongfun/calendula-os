//! Placeholder for the optional compile-in custom font module.
//!
//! Normal builds do not compile this module. To enable `builtin-custom-font`,
//! replace this file with the output of `tools/font_pack_to_rust.py`.

compile_error!(
    "builtin-custom-font requires a generated display/src/custom_generated.rs; \
     run tools/font_pack_to_rust.py with a CUSTOM.FNT pack"
);
