export type Color = 'white' | 'black'
export type PieceKind = 'king' | 'queen' | 'rook' | 'bishop' | 'knight' | 'pawn'

export type Piece = {
  color: Color
  kind: PieceKind
  name: string
  symbol: string
}

export type Board = Record<string, Piece | null>

export type MoveRecord = {
  from: string
  to: string
  piece: string
  capture?: string
  promotion?: PieceKind
  notation: string
}

export type GameStatus = {
  turn: Color
  check: Color | null
  checkmate: Color | null
  stalemate: boolean
  winner: Color | null
  message: string
}

export type GameState = {
  board: Board
  turn: Color
  history: MoveRecord[]
  status: GameStatus
}

export type MoveResult =
  | { ok: true; state: GameState; move: MoveRecord }
  | { ok: false; state: GameState; error: string }

const files = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'] as const
const ranks = ['1', '2', '3', '4', '5', '6', '7', '8'] as const

const symbols: Record<Color, Record<PieceKind, string>> = {
  white: { king: 'K', queen: 'Q', rook: 'R', bishop: 'B', knight: 'N', pawn: 'P' },
  black: { king: 'k', queen: 'q', rook: 'r', bishop: 'b', knight: 'n', pawn: 'p' },
}

const names: Record<PieceKind, string> = {
  king: 'King',
  queen: 'Queen',
  rook: 'Rook',
  bishop: 'Bishop',
  knight: 'Knight',
  pawn: 'Pawn',
}

export function makePiece(color: Color, kind: PieceKind): Piece {
  return {
    color,
    kind,
    name: `${color} ${names[kind]}`,
    symbol: symbols[color][kind],
  }
}

export function opposite(color: Color): Color {
  return color === 'white' ? 'black' : 'white'
}

export function allSquares(): string[] {
  const squares: string[] = []
  for (const rank of ranks) {
    for (const file of files) {
      squares.push(`${file}${rank}`)
    }
  }
  return squares
}

export function emptyBoard(): Board {
  return Object.fromEntries(allSquares().map((square) => [square, null])) as Board
}

export function getInitialBoard(): Board {
  const board = emptyBoard()
  const backRank: PieceKind[] = ['rook', 'knight', 'bishop', 'queen', 'king', 'bishop', 'knight', 'rook']
  for (const [index, kind] of backRank.entries()) {
    const file = files[index]
    board[`${file}1`] = makePiece('white', kind)
    board[`${file}2`] = makePiece('white', 'pawn')
    board[`${file}7`] = makePiece('black', 'pawn')
    board[`${file}8`] = makePiece('black', kind)
  }
  return board
}

export function createGame(board: Board = getInitialBoard(), turn: Color = 'white', history: MoveRecord[] = []): GameState {
  const cleanBoard = { ...emptyBoard(), ...board }
  return {
    board: cleanBoard,
    turn,
    history: [...history],
    status: computeStatus(cleanBoard, turn),
  }
}

export function createInitialGame(): GameState {
  return createGame()
}

export function isSquare(square: string): boolean {
  return /^[a-h][1-8]$/.test(square)
}

function toPoint(square: string): [number, number] {
  if (!isSquare(square)) {
    throw new Error(`Invalid square: ${square}`)
  }
  return [files.indexOf(square[0] as (typeof files)[number]), ranks.indexOf(square[1] as (typeof ranks)[number])]
}

function fromPoint(file: number, rank: number): string | null {
  if (file < 0 || file >= 8 || rank < 0 || rank >= 8) {
    return null
  }
  return `${files[file]}${ranks[rank]}`
}

function pieceAt(board: Board, square: string): Piece | null {
  return isSquare(square) ? board[square] ?? null : null
}

function cloneBoard(board: Board): Board {
  return { ...emptyBoard(), ...board }
}

function slideMoves(board: Board, from: string, piece: Piece, directions: Array<[number, number]>): string[] {
  const [file, rank] = toPoint(from)
  const moves: string[] = []
  for (const [df, dr] of directions) {
    let nextFile = file + df
    let nextRank = rank + dr
    while (true) {
      const target = fromPoint(nextFile, nextRank)
      if (!target) {
        break
      }
      const occupant = pieceAt(board, target)
      if (!occupant) {
        moves.push(target)
      } else {
        if (occupant.color !== piece.color) {
          moves.push(target)
        }
        break
      }
      nextFile += df
      nextRank += dr
    }
  }
  return moves
}

function pseudoMovesFor(board: Board, from: string): string[] {
  const piece = pieceAt(board, from)
  if (!piece) {
    return []
  }
  const [file, rank] = toPoint(from)
  const pushIfOpenOrCapture = (moves: string[], target: string | null) => {
    if (!target) {
      return
    }
    const occupant = pieceAt(board, target)
    if (!occupant || occupant.color !== piece.color) {
      moves.push(target)
    }
  }

  if (piece.kind === 'pawn') {
    const direction = piece.color === 'white' ? 1 : -1
    const homeRank = piece.color === 'white' ? 1 : 6
    const moves: string[] = []
    const one = fromPoint(file, rank + direction)
    if (one && !pieceAt(board, one)) {
      moves.push(one)
      const two = fromPoint(file, rank + direction * 2)
      if (rank === homeRank && two && !pieceAt(board, two)) {
        moves.push(two)
      }
    }
    for (const df of [-1, 1]) {
      const target = fromPoint(file + df, rank + direction)
      if (target) {
        const occupant = pieceAt(board, target)
        if (occupant && occupant.color !== piece.color) {
          moves.push(target)
        }
      }
    }
    return moves
  }

  if (piece.kind === 'knight') {
    const moves: string[] = []
    for (const [df, dr] of [[1, 2], [2, 1], [2, -1], [1, -2], [-1, -2], [-2, -1], [-2, 1], [-1, 2]]) {
      pushIfOpenOrCapture(moves, fromPoint(file + df, rank + dr))
    }
    return moves
  }

  if (piece.kind === 'bishop') {
    return slideMoves(board, from, piece, [[1, 1], [1, -1], [-1, 1], [-1, -1]])
  }

  if (piece.kind === 'rook') {
    return slideMoves(board, from, piece, [[1, 0], [-1, 0], [0, 1], [0, -1]])
  }

  if (piece.kind === 'queen') {
    return slideMoves(board, from, piece, [[1, 0], [-1, 0], [0, 1], [0, -1], [1, 1], [1, -1], [-1, 1], [-1, -1]])
  }

  const moves: string[] = []
  for (const [df, dr] of [[1, 0], [-1, 0], [0, 1], [0, -1], [1, 1], [1, -1], [-1, 1], [-1, -1]]) {
    pushIfOpenOrCapture(moves, fromPoint(file + df, rank + dr))
  }
  return moves
}

function attackSquaresFor(board: Board, from: string): string[] {
  const piece = pieceAt(board, from)
  if (!piece) {
    return []
  }
  const [file, rank] = toPoint(from)
  if (piece.kind === 'pawn') {
    const direction = piece.color === 'white' ? 1 : -1
    return [fromPoint(file - 1, rank + direction), fromPoint(file + 1, rank + direction)].filter((square): square is string => Boolean(square))
  }
  return pseudoMovesFor(board, from)
}

export function isSquareAttacked(board: Board, square: string, byColor: Color): boolean {
  return allSquares().some((from) => {
    const piece = pieceAt(board, from)
    return piece?.color === byColor && attackSquaresFor(board, from).includes(square)
  })
}

export function findKing(board: Board, color: Color): string | null {
  return allSquares().find((square) => {
    const piece = pieceAt(board, square)
    return piece?.kind === 'king' && piece.color === color
  }) ?? null
}

export function isInCheck(board: Board, color: Color): boolean {
  const king = findKing(board, color)
  return king ? isSquareAttacked(board, king, opposite(color)) : false
}

function boardAfterMove(board: Board, from: string, to: string): Board {
  const next = cloneBoard(board)
  const piece = next[from]
  next[from] = null
  if (piece?.kind === 'pawn' && (to.endsWith('8') || to.endsWith('1'))) {
    next[to] = makePiece(piece.color, 'queen')
  } else {
    next[to] = piece ?? null
  }
  return next
}

function wouldLeaveKingInCheck(board: Board, color: Color, from: string, to: string): boolean {
  return isInCheck(boardAfterMove(board, from, to), color)
}

export function legalMovesFor(state: GameState, from: string): string[] {
  const piece = pieceAt(state.board, from)
  if (!piece || piece.color !== state.turn) {
    return []
  }
  return pseudoMovesFor(state.board, from).filter((to) => !wouldLeaveKingInCheck(state.board, piece.color, from, to))
}

function hasAnyLegalMove(board: Board, color: Color): boolean {
  const state: GameState = {
    board: cloneBoard(board),
    turn: color,
    history: [],
    status: {
      turn: color,
      check: null,
      checkmate: null,
      stalemate: false,
      winner: null,
      message: `${color} to move`,
    },
  }
  return allSquares().some((square) => legalMovesFor(state, square).length > 0)
}

export function computeStatus(board: Board, turn: Color): GameStatus {
  const check = isInCheck(board, turn) ? turn : null
  const canMove = hasAnyLegalMove(board, turn)
  const checkmate = check && !canMove ? turn : null
  const stalemate = !check && !canMove
  const winner = checkmate ? opposite(checkmate) : null
  const message = checkmate
    ? `${winner} wins by checkmate`
    : stalemate
      ? 'Stalemate'
      : check
        ? `${turn} is in check`
        : `${turn} to move`
  return { turn, check, checkmate, stalemate, winner, message }
}

export function movePiece(state: GameState, from: string, to: string): MoveResult {
  const piece = pieceAt(state.board, from)
  if (!piece) {
    return { ok: false, state, error: `No piece on ${from}` }
  }
  if (piece.color !== state.turn) {
    return { ok: false, state, error: `It is ${state.turn}'s turn` }
  }
  if (!legalMovesFor(state, from).includes(to)) {
    return { ok: false, state, error: `Illegal move ${from}-${to}` }
  }
  const captured = pieceAt(state.board, to)
  const nextBoard = boardAfterMove(state.board, from, to)
  const promoted = piece.kind === 'pawn' && (to.endsWith('8') || to.endsWith('1'))
  const nextTurn = opposite(piece.color)
  const move: MoveRecord = {
    from,
    to,
    piece: piece.name,
    capture: captured?.name,
    promotion: promoted ? 'queen' : undefined,
    notation: `${piece.symbol}${from}-${to}${captured ? 'x' : ''}${promoted ? '=Q' : ''}`,
  }
  const nextState: GameState = {
    board: nextBoard,
    turn: nextTurn,
    history: [...state.history, move],
    status: computeStatus(nextBoard, nextTurn),
  }
  return { ok: true, state: nextState, move }
}
