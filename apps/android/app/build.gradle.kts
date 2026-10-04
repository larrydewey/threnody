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
        versionCode = 1
        versionName = "0.1.0"
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
