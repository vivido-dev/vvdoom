const DOOMGENERIC_SOURCES: &[&str] = &[
    "vendor/doomgeneric/dummy.c",
    "vendor/doomgeneric/am_map.c",
    "vendor/doomgeneric/doomdef.c",
    "vendor/doomgeneric/doomstat.c",
    "vendor/doomgeneric/dstrings.c",
    "vendor/doomgeneric/d_event.c",
    "vendor/doomgeneric/d_items.c",
    "vendor/doomgeneric/d_iwad.c",
    "vendor/doomgeneric/d_loop.c",
    "vendor/doomgeneric/d_main.c",
    "vendor/doomgeneric/d_mode.c",
    "vendor/doomgeneric/d_net.c",
    "vendor/doomgeneric/f_finale.c",
    "vendor/doomgeneric/f_wipe.c",
    "vendor/doomgeneric/g_game.c",
    "vendor/doomgeneric/hu_lib.c",
    "vendor/doomgeneric/hu_stuff.c",
    "vendor/doomgeneric/info.c",
    "vendor/doomgeneric/i_cdmus.c",
    "vendor/doomgeneric/i_endoom.c",
    "vendor/doomgeneric/i_joystick.c",
    "vendor/doomgeneric/i_scale.c",
    "vendor/doomgeneric/i_sound.c",
    "vendor/doomgeneric/i_system.c",
    "vendor/doomgeneric/i_timer.c",
    "vendor/doomgeneric/memio.c",
    "vendor/doomgeneric/m_argv.c",
    "vendor/doomgeneric/m_bbox.c",
    "vendor/doomgeneric/m_cheat.c",
    "vendor/doomgeneric/m_config.c",
    "vendor/doomgeneric/m_controls.c",
    "vendor/doomgeneric/m_fixed.c",
    "vendor/doomgeneric/m_menu.c",
    "vendor/doomgeneric/m_misc.c",
    "vendor/doomgeneric/m_random.c",
    "vendor/doomgeneric/p_ceilng.c",
    "vendor/doomgeneric/p_doors.c",
    "vendor/doomgeneric/p_enemy.c",
    "vendor/doomgeneric/p_floor.c",
    "vendor/doomgeneric/p_inter.c",
    "vendor/doomgeneric/p_lights.c",
    "vendor/doomgeneric/p_map.c",
    "vendor/doomgeneric/p_maputl.c",
    "vendor/doomgeneric/p_mobj.c",
    "vendor/doomgeneric/p_plats.c",
    "vendor/doomgeneric/p_pspr.c",
    "vendor/doomgeneric/p_saveg.c",
    "vendor/doomgeneric/p_setup.c",
    "vendor/doomgeneric/p_sight.c",
    "vendor/doomgeneric/p_spec.c",
    "vendor/doomgeneric/p_switch.c",
    "vendor/doomgeneric/p_telept.c",
    "vendor/doomgeneric/p_tick.c",
    "vendor/doomgeneric/p_user.c",
    "vendor/doomgeneric/r_bsp.c",
    "vendor/doomgeneric/r_data.c",
    "vendor/doomgeneric/r_draw.c",
    "vendor/doomgeneric/r_main.c",
    "vendor/doomgeneric/r_plane.c",
    "vendor/doomgeneric/r_segs.c",
    "vendor/doomgeneric/r_sky.c",
    "vendor/doomgeneric/r_things.c",
    "vendor/doomgeneric/sha1.c",
    "vendor/doomgeneric/sounds.c",
    "vendor/doomgeneric/statdump.c",
    "vendor/doomgeneric/st_lib.c",
    "vendor/doomgeneric/st_stuff.c",
    "vendor/doomgeneric/s_sound.c",
    "vendor/doomgeneric/tables.c",
    "vendor/doomgeneric/v_video.c",
    "vendor/doomgeneric/wi_stuff.c",
    "vendor/doomgeneric/w_checksum.c",
    "vendor/doomgeneric/w_file.c",
    "vendor/doomgeneric/w_main.c",
    "vendor/doomgeneric/w_wad.c",
    "vendor/doomgeneric/z_zone.c",
    "vendor/doomgeneric/w_file_stdc.c",
    "vendor/doomgeneric/i_input.c",
    "vendor/doomgeneric/i_video.c",
    "vendor/doomgeneric/doomgeneric.c",
];

fn main() {
    let mut build = cc::Build::new();
    build
        .include("vendor/doomgeneric")
        .include("vendor/miniaudio")
        // Vendored doomtype.h defines true/false as enum constants, which is
        // illegal under the C23 default of GCC 15+; pin the older standard.
        .std("gnu11")
        .define("FEATURE_SOUND", None)
        .define("_THREADSAFE", None)
        .warnings(false)
        .flag_if_supported("-fno-sanitize=undefined")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers");

    for source in DOOMGENERIC_SOURCES {
        build.file(source);
        println!("cargo:rerun-if-changed={source}");
    }

    build.file("vendor/miniaudio/doom_miniaudio_sound_bridge.c");
    println!("cargo:rerun-if-changed=vendor/miniaudio/doom_miniaudio_sound_bridge.c");
    println!("cargo:rerun-if-changed=vendor/miniaudio/miniaudio.h");

    build.compile("doomgeneric");

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=AudioToolbox");
    } else if cfg!(target_family = "unix") {
        println!("cargo:rustc-link-lib=m");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=pthread");
    }
}
