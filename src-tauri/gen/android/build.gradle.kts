buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:9.3.1")
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.2.10")
    }
}

allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

tasks.register("clean").configure {
    delete("build")
}


// Only android-36 and build-tools 36.1.0 are installed. Library modules
// (blec's Android module declares compileSdk 34) compile against 36 instead
// of pulling in another platform. Same as the gating spike.
subprojects {
    plugins.withId("com.android.library") {
        extensions.getByType(com.android.build.api.variant.LibraryAndroidComponentsExtension::class.java)
            .finalizeDsl { it.compileSdk = 36; it.buildToolsVersion = "36.1.0" }
    }
}
