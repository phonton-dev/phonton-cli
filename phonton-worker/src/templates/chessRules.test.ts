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

function runRulesSeedTests() {
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

  const blocked = movePiece(initial, 'e2', 'e5')
  assert(!blocked.ok, 'illegal pawn move is rejected')

  const captureBoard = createGame({
    board: {
      ...emptyBoard(),
      e2: makePiece('pawn', 'white'),
      d7: makePiece('pawn', 'black'),
      e1: makePiece('king', 'white'),
      e8: makePiece('king', 'black'),
    },
    turn: 'white',
  })
  const capture = movePiece(captureBoard, 'e2', 'd7')
  assert(capture.ok, 'capture move succeeds')
  assert(capture.state.board.d7?.kind === 'pawn', 'captured pawn is removed')

  const checkBoard = createGame({
    board: {
      ...emptyBoard(),
      e1: makePiece('king', 'white'),
      e8: makePiece('king', 'black'),
      d8: makePiece('queen', 'black'),
    },
    turn: 'black',
  })
  assert(isInCheck(checkBoard, 'white'), 'king in check is detected')

  const promotionBoard = createGame({
    board: {
      ...emptyBoard(),
      a7: makePiece('pawn', 'white'),
      a8: null,
      e1: makePiece('king', 'white'),
      e8: makePiece('king', 'black'),
    },
    turn: 'white',
  })
  const promotion = movePiece(promotionBoard, 'a7', 'a8')
  assert(promotion.ok, 'promotion move succeeds')
  assert(promotion.state.board.a8?.kind === 'queen', 'pawn promotes to queen')
  assert(promotion.move.promotion === 'queen', 'promotion is recorded')
}

runRulesSeedTests()
