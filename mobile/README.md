# RpiChat (Android)

LAN Chat Android app in `mobile/`. Tokens stay in AsyncStorage (`rpi-mobile-chat-tokens:v1`), never in the runner origin.

## Build the release APK

From `mobile/`:

```bash
npx tsc --noEmit
npx jest --ci
cd android && ./gradlew assembleRelease
```

APK: `android/app/build/outputs/apk/release/rpi-chat-<versionName>.apk` (see `RELEASE.md` for artifact verification).

The release APK contains its JS bundle and does not require Metro. The release variant is signed with the upload keystore from `android/keystore.properties`, not the debug certificate. It is for sideload installation, not store publication.

## Install on a device or emulator

USB debugging on, then from `mobile/`:

```bash
"$ANDROID_HOME/platform-tools/adb" devices
"$ANDROID_HOME/platform-tools/adb" install -r android/app/build/outputs/apk/release/rpi-chat-1.0.0.apk
```

If `adb` is on `PATH`, the same commands can use `adb` directly.

A physical device or booted emulator must appear as `device` in `adb devices` before installation. A listed `offline` device is not ready.

## Self-signed runner certificates

On a cert error, tap **Trust certificate**, confirm the SHA-256 fingerprint in-app, then reconnect. Do not use a system-browser exception as the trust path.

# Getting Started (React Native template)

>**Note**: Make sure you have completed the [React Native - Environment Setup](https://reactnative.dev/docs/environment-setup) instructions till "Creating a new application" step, before proceeding.

## Step 1: Start the Metro Server

First, you will need to start **Metro**, the JavaScript _bundler_ that ships _with_ React Native.

To start Metro, run the following command from the _root_ of your React Native project:

```bash
# using npm
npm start

# OR using Yarn
yarn start
```

## Step 2: Start your Application

Let Metro Bundler run in its _own_ terminal. Open a _new_ terminal from the _root_ of your React Native project. Run the following command to start your _Android_ or _iOS_ app:

### For Android

```bash
# using npm
npm run android

# OR using Yarn
yarn android
```

### For iOS

```bash
# using npm
npm run ios

# OR using Yarn
yarn ios
```

If everything is set up _correctly_, you should see your new app running in your _Android Emulator_ or _iOS Simulator_ shortly provided you have set up your emulator/simulator correctly.

This is one way to run your app — you can also run it directly from within Android Studio and Xcode respectively.

## Step 3: Modifying your App

Now that you have successfully run the app, let's modify it.

1. Open `App.tsx` in your text editor of choice and edit some lines.
2. For **Android**: Press the <kbd>R</kbd> key twice or select **"Reload"** from the **Developer Menu** (<kbd>Ctrl</kbd> + <kbd>M</kbd> (on Window and Linux) or <kbd>Cmd ⌘</kbd> + <kbd>M</kbd> (on macOS)) to see your changes!

   For **iOS**: Hit <kbd>Cmd ⌘</kbd> + <kbd>R</kbd> in your iOS Simulator to reload the app and see your changes!

## Congratulations! :tada:

You've successfully run and modified your React Native App. :partying_face:

### Now what?

- If you want to add this new React Native code to an existing application, check out the [Integration guide](https://reactnative.dev/docs/integration-with-existing-apps).
- If you're curious to learn more about React Native, check out the [Introduction to React Native](https://reactnative.dev/docs/getting-started).

# Troubleshooting

If you can't get this to work, see the [Troubleshooting](https://reactnative.dev/docs/troubleshooting) page.

# Learn More

To learn more about React Native, take a look at the following resources:

- [React Native Website](https://reactnative.dev) - learn more about React Native.
- [Getting Started](https://reactnative.dev/docs/environment-setup) - an **overview** of React Native and how setup your environment.
- [Learn the Basics](https://reactnative.dev/docs/getting-started) - a **guided tour** of the React Native **basics**.
- [Blog](https://reactnative.dev/blog) - read the latest official React Native **Blog** posts.
- [`@facebook/react-native`](https://github.com/facebook/react-native) - the Open Source; GitHub **repository** for React Native.
