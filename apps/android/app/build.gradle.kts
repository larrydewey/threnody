plugins {
    id("com.android.application")
}

android {
    namespace = "org.threnody.app"
    compileSdk = 36
    defaultConfig {
        applicationId = "org.threnody.app"
        minSdk = 29
        targetSdk = 36
        // CI sets these from the tag. Local builds use the same scheme
        // (major * 1000000 + minor * 1000 + patch), so a build from the
        // laptop installs over a release instead of being refused as a
        // downgrade.
        versionName = System.getenv("THRENODY_VERSION") ?: "0.5.2"
        versionCode = System.getenv("THRENODY_VERSION_CODE")?.toInt()
            ?: versionName!!.substringBefore('-').split('.').map(String::toInt)
                .let { (major, minor, patch) -> major * 1_000_000 + minor * 1_000 + patch }
        // GIF search's GIPHY key: from CI's GIPHY_API_KEY secret, or a
        // `giphyApiKey` Gradle property. Without one, the app asks for it.
        val giphy = System.getenv("GIPHY_API_KEY") ?: (project.findProperty("giphyApiKey") as String?) ?: ""
        buildConfigField("String", "GIPHY_API_KEY", "\"${giphy.replace("\"", "")}\"")
    }
    buildFeatures {
        buildConfig = true
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

dependencies {
    // UniFFI's Kotlin bindings call into libthrenody_ffi.so through JNA.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
}
