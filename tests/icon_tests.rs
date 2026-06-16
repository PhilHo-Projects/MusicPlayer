use image::GenericImageView;
use music_player::icon::app_icon;

#[test]
fn bundled_app_icon_is_small_square_image() {
    let bytes = std::fs::read("assets/icon.png").expect("app icon png should be bundled");
    let image = image::load_from_memory(&bytes).expect("app icon png should decode");

    assert_eq!(image.dimensions(), (64, 64));
    assert!(bytes.len() < 10_000, "icon should stay lightweight");
}

#[test]
fn app_icon_loader_returns_valid_egui_icon_data() {
    let icon = app_icon();

    assert_eq!(icon.width, 64);
    assert_eq!(icon.height, 64);
    assert_eq!(icon.rgba.len(), 64 * 64 * 4);
}

#[test]
fn bundled_app_icon_reads_as_music_note_not_play_button() {
    let bytes = std::fs::read("assets/icon.png").expect("app icon png should be bundled");
    let image = image::load_from_memory(&bytes)
        .expect("app icon png should decode")
        .into_rgba8();

    assert!(is_dark(image.get_pixel(20, 20).0));
    assert!(is_light(image.get_pixel(42, 32).0));
}

fn is_dark(rgba: [u8; 4]) -> bool {
    rgba[3] > 240 && rgba[0] < 20 && rgba[1] < 20 && rgba[2] < 20
}

fn is_light(rgba: [u8; 4]) -> bool {
    rgba[3] > 240 && rgba[0] > 235 && rgba[1] > 235 && rgba[2] > 235
}
