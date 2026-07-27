export class SessionStreamProtocolError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'SessionStreamProtocolError'
  }
}
