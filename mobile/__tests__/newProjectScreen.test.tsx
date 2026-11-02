import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { DARK } from '../src/ui';

jest.mock('../src/runtime', () => ({
  mappingStore: {
    addProject: jest.fn(async () => {
      throw new Error('addProject should not run');
    }),
    mapRunner: jest.fn(async () => {}),
  },
  registry: {},
}));

import { mappingStore } from '../src/runtime';
import { NewProjectScreen } from '../src/screens/ProjectsScreen';

it('rejects a relative project cwd before addProject', async () => {
  const errors: string[] = [];
  const created = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(
      <NewProjectScreen
        palette={DARK}
        runners={[]}
        onError={(message) => errors.push(message)}
        onCreated={created}
        onClose={() => {}}
      />,
    );
  });
  await act(async () => {
    tree.root.findByProps({ placeholder: 'Label' }).props.onChangeText('app');
  });
  await act(async () => {
    tree.root.findByProps({ placeholder: '/workspace/project' }).props.onChangeText('tmp/proj');
  });
  const confirm = tree.root.findByProps({ accessibilityLabel: 'Confirm' });
  expect(confirm.props.disabled).toBe(false);
  await act(async () => {
    confirm.props.onPress();
  });
  expect(errors).toEqual(['Project directory must be absolute.']);
  expect(mappingStore.addProject).not.toHaveBeenCalled();
  expect(created).not.toHaveBeenCalled();
  expect(tree.root.findByProps({ placeholder: '/workspace/project' }).props.value).toBe('tmp/proj');
  expect(tree.root.findAllByType(require('react-native').Text).some((node) => node.props.children === 'Creating')).toBe(false);
  await act(async () => {
    tree.unmount();
  });
});
