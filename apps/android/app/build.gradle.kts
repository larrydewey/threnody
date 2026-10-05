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
        // Release builds take these from CI (the tag and run number).
        versionCode = System.getenv("THRENODY_VERSION_CODE")?.toInt() ?: 1
        versionName = System.getenv("THRENODY_VERSION") ?: "0.1.0"
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
