//
//  SignInWithAppleAuthorizationController.swift
//  tauri-plugin-sign-in-with-apple
//
//  Created by Manaf Mhamdi Alaoui on 10.11.2024.
//

import AuthenticationServices
import Tauri

class SignInWithAppleAuthorizationController: NSObject, ASAuthorizationControllerDelegate {
    var invoke: Invoke
    init(_ invoke: Invoke) {
        self.invoke = invoke
    }
    
    public func authorize(req: AppleIDAuthorizationRequest) {
        let provider = ASAuthorizationAppleIDProvider()
        let request = provider.createRequest()
        
        if let nonce = req.nonce {
            request.nonce = nonce
        }
        if let state = req.state {
            request.state = state
        }
        
        request.requestedScopes = []
        for scope in req.scope {
            switch scope {
            case "email":
                request.requestedScopes?.append(.email)
            case "fullName":
                request.requestedScopes?.append(.fullName)
            default:
                NSLog("[tauri-plugin-sign-in-with-apple] Unsupported scope: \(scope)")
                continue;
            }
        }
        
        let controller = ASAuthorizationController(
            authorizationRequests: [
                request
            ]
        )
        
        controller.delegate = self
        controller.performRequests()
    }
    
    public func authorizationController(
        controller: ASAuthorizationController,
        didCompleteWithAuthorization authorization: ASAuthorization
    ) {
        if let appleIDCredential = authorization.credential as? ASAuthorizationAppleIDCredential {
            invoke.resolve(
                AppleIDAuthorizationResponse(
                    userIdentifier: !appleIDCredential.user.isEmpty ?
                        appleIDCredential.user :
                        nil,
                    givenName: appleIDCredential.fullName?.givenName,
                    familyName: appleIDCredential.fullName?.familyName,
                    email: appleIDCredential.email,
                    authorizationCode: String(decoding: appleIDCredential.authorizationCode!, as: UTF8.self),
                    identityToken: appleIDCredential.identityToken != nil ?
                        String(decoding: appleIDCredential.identityToken!, as: UTF8.self) :
                        nil,
                    state: appleIDCredential.state
                )
            )
        } else {
            invoke.reject("Returned credential is not of type ASAuthorizationAppleIDCredential")
        }
    }
    
    public func authorizationController(
        controller: ASAuthorizationController,
        didCompleteWithError error: Error
    ) {
        invoke.reject(error.localizedDescription)
    }
}
