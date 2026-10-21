// A case of the API's test suite (docs/dev/compatibility.md): `nsh/android`
// off Android (the headless runtime is iOS): `isAvailable` is false, and its
// functions are callable and do nothing, so an app that calls them runs on.
// Its script
// checks each.
import { Text, View, signal } from "nsh";
import { BackHandler, Toast, isAvailable } from "nsh/android";

function App() {
  const calls = signal("not called");
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <Text>{`Android: ${isAvailable}`}</Text>
      <Text>{`Exit: ${typeof BackHandler.exitApp}`}</Text>
      <Text>{`Calls: ${calls()}`}</Text>
      <View
        testID="call"
        onPress={() => {
          const listener = BackHandler.onBackPress(() => true);
          listener();
          Toast.show("Saved");
          Toast.show("Saved for long", "long");
          calls.set(`done, remove ${typeof listener}`);
        }}
      >
        <Text>Call them</Text>
      </View>
    </View>
  );
}

export default App;
