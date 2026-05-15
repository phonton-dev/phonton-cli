import { describe, expect, it } from 'vitest'
import {
  createGame,
  createInitialGame,
  emptyBoard,
  isInCheck,
  legalMovesFor,
  makePiece,
  movePiece,
} from './chessRules'

describe('chess rules boundary', () => {
  it('creates the standard starting position', () => {
    const game = createInitialGame()
    const pieceCount = Object.values(game.board).filter(Boolean).length

    expect(pieceCount).toBe(32)
    expect(game.turn).toBe('white')
    expect(game.board.e1?.kind).toBe('king')
    expect(game.board.e8?.kind).toBe('king')
  })

  it('allows normal pawn and knight moves and enforces turn order', () => {
    const game = createInitialGame()

    expect(legalMovesFor(game, 'e2')).toContain('e4')
    expect(legalMovesFor(game, 'g1')).toContain('f3')

    const moved = movePiece(game, 'e2', 'e4')
    expect(moved.ok).toBe(true)
    if (moved.ok) {
      expect(moved.state.turn).toBe('black')
      expect(movePiece(moved.state, 'e4', 'e5').ok).toBe(false)
    }
  })

  it('rejects blocked and illegal moves', () => {
    const game = createInitialGame()

    expect(legalMovesFor(game, 'a1')).not.toContain('a4')
    expect(movePiece(game, 'e2', 'e5').ok).toBe(false)
  })

  it('detects check and filters king moves into attacked squares', () => {
    const board = emptyBoard()
    board.e1 = makePiece('white', 'king')
    board.a8 = makePiece('black', 'king')
    board.e8 = makePiece('black', 'rook')
    const game = createGame(board, 'white')

    expect(isInCheck(game.board, 'white')).toBe(true)
    expect(legalMovesFor(game, 'e1')).not.toContain('e2')
  })

  it('promotes pawns to queens', () => {
    const board = emptyBoard()
    board.e1 = makePiece('white', 'king')
    board.e8 = makePiece('black', 'king')
    board.a7 = makePiece('white', 'pawn')
    const game = createGame(board, 'white')

    const result = movePiece(game, 'a7', 'a8')

    expect(result.ok).toBe(true)
    if (result.ok) {
      expect(result.state.board.a8?.kind).toBe('queen')
      expect(result.move.promotion).toBe('queen')
    }
  })
})
