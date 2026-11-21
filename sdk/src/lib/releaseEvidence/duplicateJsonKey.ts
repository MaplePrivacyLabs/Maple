/*
 * JSON.parse keeps the last of two members that share a name, so a document
 * that names one member twice can be read differently by different parsers.
 * Release evidence with a repeated member name is rejected before anything
 * in it is trusted.
 *
 * JSON.parse checks the grammar first. Once it has accepted the text, every
 * string and every structural character sits where the grammar puts it, so
 * one pass over string and bracket tokens with a stack sees every member name
 * in the object it belongs to. Nothing here parses JSON by hand.
 */

// One string token, escapes included, or one structural character.
const TOKEN = /"(?:[^"\\]|\\.)*"|[{}[\]:]/g;

/**
 * Whether any object in the text names the same member twice. Escaped
 * spellings of one name count as the same name, as they do for JSON.parse.
 * Throws a SyntaxError when the text is not JSON.
 */
export function hasDuplicateJsonMember(text: string): boolean {
  JSON.parse(text);
  const tokens = text.match(TOKEN) ?? [];
  // One entry per open container: the member names seen so far in an object,
  // or undefined for an array.
  const openContainers: (Set<string> | undefined)[] = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    const opensObject = token === "{";
    if (opensObject) {
      openContainers.push(new Set());
      continue;
    }
    const opensArray = token === "[";
    if (opensArray) {
      openContainers.push(undefined);
      continue;
    }
    const closesContainer = token === "}" || token === "]";
    if (closesContainer) {
      openContainers.pop();
      continue;
    }
    // In JSON, only a member name is followed by a colon.
    const isString = token.startsWith('"');
    const followedByColon = tokens[index + 1] === ":";
    const isMemberName = isString && followedByColon;
    if (!isMemberName) {
      continue;
    }
    // A member name only occurs inside an object, so the innermost open
    // container is that object's set of names.
    const namesInObject = openContainers[openContainers.length - 1] as Set<string>;
    const name = JSON.parse(token) as string;
    const repeated = namesInObject.has(name);
    if (repeated) {
      return true;
    }
    namesInObject.add(name);
  }
  return false;
}
