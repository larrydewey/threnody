plugins {
    id("com.android.application")
}

// Calls need LiveKit's prebuilt WebRTC; `-Pthrenody.calls=false` (the
// F-Droid build, `build.sh --no-calls`) builds without them.
val calls = (findProperty("threnody.calls") as String?)?.toBooleanStrict() ?: true

android {
    namespace = "org.threnody.app"
    compileSdk = 36
    defaultConfig {
        applicationId = "org.threnody.app"
        minSdk = 29
        targetSdk = 36
        // Literal, so F-Droid's update checker can read them. They must equal
        // the workspace version in Cargo.toml (checked below; the code is
        // major * 1000000 + minor * 1000 + patch, so a build from the laptop
        // installs over a release); each release's "prepare" commit bumps both.
        versionCode = 1000000
        versionName = "1.0.0"
        val workspace = workspaceVersion()
        check(versionName == workspace && versionCode == codeOf(workspace)) {
            "app/build.gradle.kts has $versionName ($versionCode) but Cargo.toml has $workspace " +
                "(${codeOf(workspace)}): update both"
        }
        // CI's prerelease tags (0.18.0-rc.1) keep their suffix in the name.
        System.getenv("THRENODY_VERSION")?.let { versionName = it }
        System.getenv("THRENODY_VERSION_CODE")?.let { versionCode = it.toInt() }
        // GIF search's GIPHY key: from CI's GIPHY_API_KEY secret, or a
        // `giphyApiKey` Gradle property. Without one, the app asks for it.
        val giphy = System.getenv("GIPHY_API_KEY") ?: (project.findProperty("giphyApiKey") as String?) ?: ""
        buildConfigField("String", "GIPHY_API_KEY", "\"${giphy.replace("\"", "")}\"")
        // Only the ABIs build.sh built libthrenody_ffi.so for; JNA alone ships
        // others, and an APK listing them would install on phones it can't run on.
        jniAbis().takeIf { it.isNotEmpty() }?.let { ndk { abiFilters += it } }
    }
    buildFeatures {
        buildConfig = true
    }
    // No dependency metadata blob (encrypted to Google's key) in the APK.
    dependenciesInfo {
        includeInApk = false
        includeInBundle = false
    }
    // Release signing comes only from the environment (CI secrets); the
    // key never lives in the repository. Without it, release builds are
    // left unsigned.
    val keystore = System.getenv("THRENODY_KEYSTORE")?.let(::file)?.takeIf { it.exists() }
    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = keystore
                storePassword = System.getenv("THRENODY_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("THRENODY_KEY_ALIAS")
                keyPassword = System.getenv("THRENODY_KEY_PASSWORD")
            }
        }
    }
    buildTypes {
        release {
            // UniFFI's bindings reach the library through JNA by reflection.
            isMinifyEnabled = false
            if (keystore != null) signingConfig = signingConfigs.getByName("release")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

// Without calls, a stand-in for the one WebRTC class Calls.kt names, and
// a manifest overlay that drops what only calls need.
androidComponents {
    onVariants { variant ->
        if (!calls) {
            variant.sources.kotlin?.addStaticSourceDirectory("src/nocalls/java")
            variant.sources.manifests.addStaticManifestFile("src/nocalls/AndroidManifest.xml")
        }
    }
}

dependencies {
    // UniFFI's Kotlin bindings call into libthrenody_ffi.so through JNA.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    // Material 3 Components for dynamic color support
    implementation("com.google.android.material:material:1.12.0")
    // AndroidX Core for AppCompatDelegate
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.core:core-ktx:1.19.1")
    // UnifiedPush wake-ups (Appendix R), off unless the user turns them on.
    implementation("org.unifiedpush.android:connector:3.3.5")
    // WebRTC's Java half for calls (audio devices), from LiveKit's prebuilt
    // WebRTC; build.sh copies it here.
    if (calls) implementation(files("libs/libwebrtc.jar"))
}

/** The `[workspace.package]` version in the repository's Cargo.toml. */
fun workspaceVersion(): String {
    val toml = rootDir.resolve("../../Cargo.toml").readText()
    val section = toml.substringAfter("[workspace.package]").substringBefore("\n[")
    return Regex("""(?m)^version = "([^"]+)"""").find(section)?.groupValues?.get(1)
        ?: error("no workspace version in Cargo.toml")
}

/** The versionCode for `version`: major * 1000000 + minor * 1000 + patch. */
fun codeOf(version: String): Int = version.substringBefore('-').split('.').map(String::toInt)
    .let { (major, minor, patch) -> major * 1_000_000 + minor * 1_000 + patch }

/** ABIs with a native library in src/main/jniLibs, sorted. */
fun jniAbis(): List<String> = file("src/main/jniLibs").listFiles()
    ?.filter { it.isDirectory && it.resolve("libthrenody_ffi.so").exists() }
    ?.map { it.name }?.sorted().orEmpty()
