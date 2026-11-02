jest.mock('@react-native-async-storage/async-storage', () =>
  require('@react-native-async-storage/async-storage/jest/async-storage-mock'),
);
jest.mock('react-native-vector-icons/MaterialIcons', () => {
  const React = require('react');
  const { Text } = require('react-native');
  return function Icon(props) {
    return React.createElement(Text, { accessibilityLabel: props.name }, props.name);
  };
});
jest.mock('react-native-webview', () => {
  const React = require('react');
  const { View } = require('react-native');
  return { WebView: () => React.createElement(View) };
});
jest.mock('react-native-camera-kit', () => {
  const React = require('react');
  const { View } = require('react-native');
  return {
    Camera: () => React.createElement(View),
    CameraType: { Back: 'back' },
  };
});
