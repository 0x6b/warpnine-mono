//! Configuration constants for Warpnine font builds.

/// Recursive VF source font filename.
pub const RECURSIVE_VF_FILENAME: &str = "Recursive_VF_1.085.ttf";

/// Noto Sans Mono CJK JP VF source font filename.
pub const NOTO_CJK_VF_FILENAME: &str = "NotoSansMonoCJKjp-VF.ttf";

/// JetBrains Mono Regular filename (for box drawing characters).
pub const JETBRAINS_MONO_FILENAME: &str = "JetBrainsMono-Regular.ttf";

/// Recursive font version.
pub const RECURSIVE_VERSION: &str = "1.085";

/// JetBrains Mono version.
pub const JETBRAINS_MONO_VERSION: &str = "2.304";

/// Recursive VF download URL (ZIP archive).
pub const RECURSIVE_ZIP_URL: &str =
    "https://github.com/arrowtype/recursive/releases/download/v1.085/ArrowType-Recursive-1.085.zip";

/// Path to the VF font inside the Recursive ZIP archive.
pub const RECURSIVE_ZIP_PATH: &str =
    "ArrowType-Recursive-1.085/Recursive_Desktop/Recursive_VF_1.085.ttf";

/// JetBrains Mono download URL (ZIP archive).
pub const JETBRAINS_MONO_ZIP_URL: &str =
    "https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip";

/// Path to the Regular font inside the JetBrains Mono ZIP archive.
pub const JETBRAINS_MONO_ZIP_PATH: &str = "fonts/ttf/JetBrainsMono-Regular.ttf";

/// Noto CJK commit hash for reproducible builds.
pub const NOTO_CJK_COMMIT: &str = "f8d157532fbfaeda587e826d4cd5b21a49186f7c";

/// Noto Sans Mono CJK JP VF download URL.
pub const NOTO_CJK_VF_URL: &str = "https://raw.githubusercontent.com/notofonts/noto-cjk/f8d157532fbfaeda587e826d4cd5b21a49186f7c/Sans/Variable/TTF/Mono/NotoSansMonoCJKjp-VF.ttf";

/// Noto CJK license download URL.
pub const NOTO_CJK_LICENSE_URL: &str = "https://raw.githubusercontent.com/notofonts/noto-cjk/f8d157532fbfaeda587e826d4cd5b21a49186f7c/Sans/LICENSE";

/// Recursive license download URL.
pub const RECURSIVE_LICENSE_URL: &str =
    "https://raw.githubusercontent.com/arrowtype/recursive/refs/tags/v1.085/OFL.txt";

/// JetBrains Mono license download URL.
pub const JETBRAINS_MONO_LICENSE_URL: &str =
    "https://raw.githubusercontent.com/JetBrains/JetBrainsMono/v2.304/OFL.txt";

pub const NOTO_CJK_VF_SHA256: &str =
    "9a91b2f42ad958fd4295586809f85366f0afa020b85ac70b39916c25bc5cda15";
pub const RECURSIVE_VF_SHA256: &str =
    "653221ca467f4732fe6856ac493f6c409e9f56a7674abe36b2364acc89796f7c";
pub const JETBRAINS_MONO_SHA256: &str =
    "a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f";
pub const NOTO_CJK_LICENSE_SHA256: &str =
    "6a73f9541c2de74158c0e7cf6b0a58ef774f5a780bf191f2d7ec9cc53efe2bf2";
pub const RECURSIVE_LICENSE_SHA256: &str =
    "f9f539cf7549bd417159dbdb9c400943a5b60a7366c2c6fbde9f095173d82479";
pub const JETBRAINS_MONO_LICENSE_SHA256: &str =
    "30f0c136e3c88e422d0791acd97238870f9054a9729bc34cf2ff0d4ed8cac4ad";
