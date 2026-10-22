// A retained row keeps local state while a splice moves its list index.
// Replacing that row still disposes the keyed child and initializes new state.
import { List, Text, View, signal } from "nsh";

function Row(props: { label: string; index: number }) {
  const selected = signal(false);
  return (
    <View testID={`select-${props.label}`} style={{ height: 40 }} onPress={() => selected.set(true)}>
      <Text>{`${props.label}/${props.index}/${selected()}`}</Text>
    </View>
  );
}

export default function App() {
  const label = signal("B");
  const a = { label: () => "A" }, b = { label }, x = { label: () => "X" };
  const rows = signal([a, b]);
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <View testID="prepend" onPress={() => rows.set([x, a, b])}><Text>Prepend</Text></View>
      <View testID="remove" onPress={() => rows.set([a, b])}><Text>Remove</Text></View>
      <View testID="rename" onPress={() => label.set("B2")}><Text>Rename</Text></View>
      <View testID="replace" onPress={() => rows.set([a, { label: () => "C" }])}><Text>Replace</Text></View>
      <List each={rows()} style={{ height: 200 }}>
        {(item, index) => <Each of={[item()]}>{row => <Row label={row().label()} index={index()} />}</Each>}
      </List>
    </View>
  );
}
