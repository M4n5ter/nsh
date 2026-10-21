// A case of the API's test suite (docs/dev/compatibility.md): `nsh/device`
// as the headless runtime reports it (iOS, a phone, 1 pixel per point, the
// script's viewport), the color scheme an app sets, and right-to-left
// layout an app sets with its direction style. Its script checks each.
import { Text, View, signal } from "nsh";
import { platform, pixelRatio, colorScheme, fontScale, setColorScheme, windowSize } from "nsh/device";

function App() {
  const rtl = signal(false);
  return (
    <View style={{ flex: 1, paddingTop: 64 }}>
      <Text>{`OS: ${platform.os}`}</Text>
      <Text>{`Idiom: ${platform.idiom}`}</Text>
      <Text>{`Pad: ${platform.idiom === "pad"} TV: ${platform.idiom === "tv"} Catalyst: ${platform.catalyst} Desktop: ${platform.idiom === "desktop"}`}</Text>
      <Text>{`Version: ${platform.version}`}</Text>
      <Text>{`Select: ${platform.select({ ios: "ios", android: "android", default: "other" })}`}</Text>
      <Text>{`Select mobile: ${platform.select({ mobile: "mobile", desktop: "desktop" })}`}</Text>
      <Text>{`Ratio: ${pixelRatio()} font ${fontScale()}`}</Text>
      <Text>{`Pixels: ${Math.round(10 * pixelRatio())} round ${Math.round(10.4 * pixelRatio()) / pixelRatio()}`}</Text>
      <Text>{`Font scale: ${fontScale()}`}</Text>
      <Text>{`Window: ${windowSize().width}x${windowSize().height}`}</Text>
      <Text>{`Scheme: ${colorScheme()}`}</Text>
      <Text>{`Direction: ${rtl() ? "rtl" : "ltr"}`}</Text>
      <View testID="dark" onPress={() => setColorScheme("dark")}>
        <Text>Go dark</Text>
      </View>
      <View testID="system" onPress={() => setColorScheme("system")}>
        <Text>Follow the system</Text>
      </View>
      <View
        testID="rtl"
        onPress={() => rtl.set(true)}
      >
        <Text>Right to left</Text>
      </View>
      <View testID="ltr" onPress={() => rtl.set(false)}>
        <Text>Left to right</Text>
      </View>
      <View testID="row" style={{ direction: rtl() ? "rtl" : "ltr", flexDirection: "row", height: 20 }}>
        <View testID="first" style={{ width: 100, height: 20 }} />
      </View>
    </View>
  );
}

export default App;
