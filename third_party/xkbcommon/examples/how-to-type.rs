//! Translation of xkbcommon's `how-to-type` tool.

use std::process::ExitCode;

use xkbcommon::xkb;

fn main() -> ExitCode {
    let mut input_is_keysym = false;

    let mut rules = None;
    let mut model = None;
    let mut layout = None;
    let mut variant = None;
    let mut options = None;

    let mut disable_env_names = true;

    let mut input_value = None;

    let mut args_iter = std::env::args().skip(1);
    while let Some(arg) = args_iter.next() {
        if arg == "--keysym" {
            input_is_keysym = true;
        } else if arg == "--format" {
            eprintln!("--format is coming in libxkbcommon 1.11.0");
            return ExitCode::FAILURE;
        } else if arg == "--rules" {
            rules = Some(args_iter.next().expect("missing argument for --rules"));
        } else if arg == "--model" {
            model = Some(args_iter.next().expect("missing argument for --model"));
        } else if arg == "--layout" {
            layout = Some(args_iter.next().expect("missing argument for --layout"));
        } else if arg == "--variant" {
            variant = Some(args_iter.next().expect("missing argument for --variant"));
        } else if arg == "--options" {
            options = Some(args_iter.next().expect("missing argument for --options"));
        } else if arg == "--enable-environment-names" {
            disable_env_names = false;
        } else if input_value.is_some() {
            eprintln!("can't have multiple input values");
            return ExitCode::FAILURE;
        } else {
            input_value = Some(arg);
        }
    }

    let input_value = input_value.expect("missing input value");

    let target_keysym = if input_is_keysym {
        let keysym = xkb::keysym_from_name(&input_value, xkb::KEYSYM_NO_FLAGS);

        if keysym == xkb::Keysym::NoSymbol {
            // Parse as decimal
            xkb::Keysym::from(input_value.parse::<u32>().unwrap())
        } else {
            keysym
        }
    } else {
        let ch = if input_value.chars().nth(1).is_some() {
            // More than one Unicode character in input

            let parsed_int = if input_value.starts_with("U+")
                || input_value.starts_with("0x")
                || input_value.starts_with("0X")
            {
                u32::from_str_radix(&input_value[2..], 16)
            } else {
                input_value.parse()
            };

            char::from_u32(parsed_int.expect("invalid number"))
                .expect("should be a valid Unicode scalar value")
        } else {
            // Take in a Unicode scalar value as UTF-8
            input_value.chars().next().expect("empty input")
        };

        let keysym = xkb::Keysym::from_char(ch);

        if keysym == xkb::Keysym::NoSymbol {
            eprintln!("failed to convert Unicode scalar value to keysym");
            return ExitCode::FAILURE;
        }

        keysym
    };

    let context = xkb::Context::new(if disable_env_names {
        xkb::CONTEXT_NO_ENVIRONMENT_NAMES
    } else {
        xkb::CONTEXT_NO_FLAGS
    });

    let keymap = xkb::Keymap::new_from_names(
        &context,
        rules.as_deref().unwrap_or_default(),
        model.as_deref().unwrap_or_default(),
        layout.as_deref().unwrap_or_default(),
        variant.as_deref().unwrap_or_default(),
        options,
        xkb::COMPILE_NO_FLAGS,
    )
    .unwrap();

    println!(
        "keysym: {} ({:#06x})",
        target_keysym.name().expect("failed to get name of keysym"),
        target_keysym.raw()
    );
    println!(
        "{:<8} {:<9} {:<8} {:<20} {:<7} MODIFIERS",
        "KEYCODE", "KEY NAME", "LAYOUT", "LAYOUT NAME", "LEVEL#"
    );

    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();

    let num_mods = keymap.num_mods();

    for keycode in min..=max {
        let keycode = xkb::Keycode::new(keycode);

        // Skip unused keycodes
        let Some(key_name) = keymap.key_get_name(keycode) else {
            continue;
        };

        let num_layouts = keymap.num_layouts_for_key(keycode);
        for layout_index in 0..num_layouts {
            let mut layout_name = keymap.layout_get_name(layout_index);
            if layout_name.is_empty() {
                layout_name = "?";
            }

            let num_levels = keymap.num_levels_for_key(keycode, layout_index);
            for level_index in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(keycode, layout_index, level_index);

                if syms != [target_keysym] {
                    // Inequal or nonzero count
                    continue;
                };

                let mut masks = [xkb::ModMask::default(); 100];
                let num_masks =
                    keymap.key_get_mods_for_level(keycode, layout_index, level_index, &mut masks);

                let masks = &masks[0..num_masks];

                for mod_mask in masks {
                    print!(
                        "{:<8} {:<9} {:<8} {:<20} {:<7} [ ",
                        keycode.raw(),
                        key_name,
                        layout_index + 1,
                        layout_name,
                        level_index + 1
                    );

                    for mod_index in 0..num_mods {
                        // Check whether the modifier mask contains this modifier
                        if mod_mask & (1 << mod_index) == 0 {
                            continue;
                        }

                        print!("{} ", keymap.mod_get_name(mod_index));
                    }

                    println!("]");
                }
            }
        }
    }

    ExitCode::SUCCESS
}
