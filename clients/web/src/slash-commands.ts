export const SLASH_COMMAND = {
  help: '/help',
  html: '/html',
  cancel: '/cancel',
  collapse: '/collapse',
  expand: '/expand',
  create: '/create',
  dm: '/dm',
  find: '/find',
  invite: '/invite',
  jump: '/jump',
  join: '/join',
  knock: '/knock',
  leave: '/leave',
  literal: '/literal',
  pin: '/pin',
  rainbow: '/rainbow',
  react: '/react',
  refresh: '/refresh',
  reply: '/reply',
  rooms: '/rooms',
  room: '/room',
  search: '/search',
  shortcuts: '/shortcuts',
  sort: '/sort',
  spoiler: '/spoiler',
  thread: '/thread',
  unreadthreads: '/unreadthreads',
  unpin: '/unpin',
  forget: '/forget',
  whereami: '/whereami',
} as const

export type SlashCommandName =
  (typeof SLASH_COMMAND)[keyof typeof SLASH_COMMAND]

export interface SlashCommandSpec {
  name: SlashCommandName
  aliases?: readonly string[]
  usage: string
  description: string
}

export const SLASH_COMMANDS: SlashCommandSpec[] = [
  {
    name: SLASH_COMMAND.collapse,
    usage: '/collapse',
    description:
      'Hide timeline images and preview cards across rooms and threads',
  },
  {
    name: SLASH_COMMAND.expand,
    usage: '/expand',
    description:
      'Show timeline images and preview cards across rooms and threads',
  },
  {
    name: SLASH_COMMAND.html,
    usage: '/html <html>',
    description: 'Send raw HTML as a formatted message',
  },
  {
    name: SLASH_COMMAND.literal,
    usage: '/literal <text>',
    description: 'Send plaintext without Markdown formatting',
  },
  {
    name: SLASH_COMMAND.rainbow,
    usage: '/rainbow <text>',
    description: 'Send colored rainbow text',
  },
  {
    name: SLASH_COMMAND.spoiler,
    usage: '/spoiler [reason |] <text>',
    description: 'Send hidden spoiler text',
  },
  {
    name: SLASH_COMMAND.react,
    aliases: ['/+'],
    usage: '/react [emoji]',
    description: 'React to the latest visible message',
  },
  {
    name: SLASH_COMMAND.reply,
    usage: '/reply [message]',
    description: 'Reply to the latest visible message',
  },
  {
    name: SLASH_COMMAND.thread,
    usage: '/thread',
    description: 'Open a thread for the latest visible message',
  },
  {
    name: SLASH_COMMAND.room,
    aliases: ['/switch'],
    usage: '/room <room>',
    description: 'Switch rooms by name, alias, ID, or number',
  },
  {
    name: SLASH_COMMAND.dm,
    usage: '/dm [user]',
    description: 'Open direct messages, or start one with a user',
  },
  {
    name: SLASH_COMMAND.create,
    usage: '/create',
    description: 'Open the create-room form',
  },
  {
    name: SLASH_COMMAND.find,
    usage: '/find',
    description: 'Open room directory search',
  },
  {
    name: SLASH_COMMAND.invite,
    usage: '/invite <user...>',
    description: 'Invite Matrix users to the current room',
  },
  {
    name: SLASH_COMMAND.cancel,
    usage: '/cancel <user>',
    description: 'Cancel a pending room invite',
  },
  {
    name: SLASH_COMMAND.join,
    usage: '/join [room-or-matrix-link]',
    description:
      'Open room discovery, or join a room by ID, alias, or Matrix link',
  },
  {
    name: SLASH_COMMAND.knock,
    usage: '/knock <room-or-matrix-link> [reason]',
    description: 'Request access to a knockable room',
  },
  {
    name: SLASH_COMMAND.leave,
    aliases: ['/part'],
    usage: '/leave, /part',
    description: 'Leave the current room',
  },
  {
    name: SLASH_COMMAND.forget,
    usage: '/forget',
    description: 'Forget the current left or banned room',
  },
  {
    name: SLASH_COMMAND.pin,
    usage: '/pin [room]',
    description: 'Pin a room to the top of the list',
  },
  {
    name: SLASH_COMMAND.unpin,
    usage: '/unpin [room]',
    description: 'Unpin a room',
  },
  {
    name: SLASH_COMMAND.sort,
    usage: '/sort <recent|oldest|az|za>',
    description: 'Set room-list sort order',
  },
  {
    name: SLASH_COMMAND.refresh,
    aliases: ['/rooms'],
    usage: '/refresh',
    description: 'Refresh rooms',
  },
  {
    name: SLASH_COMMAND.unreadthreads,
    aliases: ['/ut'],
    usage: '/unreadthreads',
    description: 'Open unread threads',
  },
  {
    name: SLASH_COMMAND.search,
    usage: '/search [filters] [text]',
    description: 'Search messages (this room by default; room:, sender:, …)',
  },
  {
    name: SLASH_COMMAND.jump,
    usage: '/jump [YYYY-MM-DD]',
    description: 'Jump to a date, or open the date picker',
  },
  {
    name: SLASH_COMMAND.whereami,
    usage: '/whereami',
    description: 'Show room information',
  },
  {
    name: SLASH_COMMAND.help,
    aliases: ['/?'],
    usage: '/help, /?',
    description: 'Show this list',
  },
  {
    name: SLASH_COMMAND.shortcuts,
    usage: '/shortcuts',
    description: 'Show keyboard shortcuts',
  },
]

export function slashCommandSpecForInput(
  input: string,
): SlashCommandSpec | undefined {
  return SLASH_COMMANDS.find(
    (command) => command.name === input || command.aliases?.includes(input),
  )
}

export function canonicalSlashCommandName(
  input: string,
): SlashCommandName | null {
  return slashCommandSpecForInput(input)?.name ?? null
}

/** The usage line for a command, for the error a misused command answers with. */
export function slashCommandUsage(name: SlashCommandName): string {
  return SLASH_COMMANDS.find((command) => command.name === name)?.usage ?? name
}
