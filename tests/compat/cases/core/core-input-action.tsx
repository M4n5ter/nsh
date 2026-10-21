// The callback uses the captured, deeply frozen arguments even after the
// original application object changes. Its script checks the result.
import { Text, View, inputAction, signal } from "nsh";

export default function App() {
  const result = signal("pending");
  const source = { target: "original", nested: { value: 7 } };
  const activate = inputAction(
    args => result.set(`${args.target}:${args.nested.value}:${Object.isFrozen(args.nested)}`),
    source,
  );
  source.target = "changed";
  source.nested.value = 9;
  return (
    <View style={{ paddingTop: 64 }}>
      <View testID="captured-action" onPress={activate}>
        <Text>Activate captured action</Text>
      </View>
      <Text>Action: {result()}</Text>
    </View>
  );
}
