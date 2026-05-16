import {
  createGame,
  createInitialGame,
  emptyBoard,
  isInCheck,
  legalMovesFor,
  makePiece,
  movePiece,
} from './chessRules'

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message)
  }
}

function assertIncludes(values: string[], expected: string, message: string) {
  assert(values.includes(expected), message)
}

function assertExcludes(values: string[], expected: string, message: string) {
  assert(!values.includes(expected), message)
}

export function runRulesSeedTests() {
  const initial = createInitialGame()
  const pieceCount = Object.values(initial.board).filter(Boolean).length

  assert(pieceCount === 32, 'starting position has 32 pieces')
  assert(initial.turn === 'white', 'white moves first')
  assert(initial.board.e1?.kind === 'king', 'white king starts on e1')
  assert(initial.board.e8?.kind === 'king', 'black king starts on e8')

  assertIncludes(legalMovesFor(initial, 'e2'), 'e4', 'white pawn can move two squares')
  assertIncludes(legalMovesFor(initial, 'g1'), 'f3', 'white knight can move from g1 to f3')

  const moved = movePiece(initial, 'e2', 'e4')
  assert(moved.ok, 'legal pawn move succeeds')
  assert(moved.state.turn === 'black', 'turn switches after legal move')
  assert(!movePiece(moved.state, 'e4', 'e5').ok, 'turn order is enforced')

  assertExcludes(legalMovesFor(initial, 'a1'), 'a4', 'rook cannot jump blocked pawns')
  assert(!movePiece(initial, 'e2', 'e5').ok, 'illegal pawn move is rejected')

  const checkBoard = emptyBoard()
  checkBoard.e1 = makePiece('white', 'king')
  checkBoard.a8 = makePiece('black', 'king')
  checkBoard.e8 = makePiece('black', 'rook')
  const checkGame = createGame(checkBoard, 'white')
  assert(isInCheck(checkGame.board, 'white'), 'rook gives check on open file')
  assertExcludes(legalMovesFor(checkGame, 'e1'), 'e2', 'king cannot move into check')

  const promotionBoard = emptyBoard()
  promotionBoard.e1 = makePiece('white', 'king')
  promotionBoard.e8 = makePiece('black', 'king')
  promotionBoard.a7 = makePiece('white', 'pawn')
  const promotionGame = createGame(promotionBoard, 'white')
  const promotion = movePiece(promotionGame, 'a7', 'a8')

  assert(promotion.ok, 'promotion move succeeds')
  assert(promotion.state.board.a8?.kind === 'queen', 'pawn promotes to queen')
  assert(promotion.move.promotion === 'queen', 'promotion is recorded')
}

runRulesSeedTests()
