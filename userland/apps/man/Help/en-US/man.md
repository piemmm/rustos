## NAME

man — show a command's help document

## SYNOPSIS

`man [-h | -?] <command> [topic]`

## DESCRIPTION

Renders the help document a command's application bundle ships, in your
language where a translation exists.

Every TAIRiX program is an application bundle carrying a `Help/` tree: one
structured document per command or topic, per language. `man` resolves
`<command>` exactly as the shell does — the system app store first, then
the directories on `PATH` — so the page shown always documents the program
the shell would run for the same word. A trailing `.app` names the bundle
directly. When neither the store nor `PATH` holds the word, `man` searches
the app stores recursively — `/Apps` first, then the `Apps` folder in your
home — so a bundle filed away in nested folders is still found; the search
never looks inside another bundle, and the shallowest match wins.

The document is chosen for the locale in the `LANG` environment variable,
falling back to the same language in another region and finally to the
canonical English document. When the page is not shown in the requested
language, `man` notes the substitution on the advisory stream (fd 3); the
page itself is never mixed-language.

On an interactive console the page is shown a screenful at a time: space
turns the page, return advances one line, and `q` stops. When the output
is redirected or the console's size is unknown, the whole page streams.

## OPTIONS

- `-h, -?` — show this command's own short help.

## EXAMPLES

- `man ps` — show the `ps` page.
- `man top keys` — show the `keys` topic from the `top` bundle.
- `man files.app` — name the bundle directly.

## EXIT STATUS

- `0` — the page was shown.
- `1` — the command or its help document was not found, or the page could
  not be delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale (a BCP-47 tag such as `fr-FR`).
- `PATH` — the extra directories searched for `<command>.app` bundles,
  after the system app store.
- `HOME` — names your own `Apps` folder for the recursive bundle search.

## SEE ALSO

- `elsh`
